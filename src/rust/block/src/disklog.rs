//! The kernel log, written to a raw disk area as each line is produced.
//!
//! This exists for one situation: a machine with no serial port, no working
//! network and therefore no netconsole, that stops somewhere in boot and says
//! nothing at all. The netconsole cannot help there -- it needs a NIC that
//! works and a link that is up, and by the time either exists the interesting
//! part is over.
//!
//! HOW A LINE GETS THERE. The tracer hands every line to `log` from whatever
//! context it runs in -- an interrupt handler, code under a spinlock, the
//! panic path -- so `log` takes no lock and never waits: the line goes into a
//! free slot, the slot onto a ready ring (`kcore::static_ring`), and that is
//! all. Until the area is known that ring is the boot log so far, and `setup`
//! writes all of it before it returns. From then on a task of its own does
//! the writing, woken by each line: a write waits for the device -- one line
//! has cost 25-33 ms on real hardware -- and whoever traced, the receive path
//! or a lock holder, is no place to wait that long. With no scheduler to hand
//! a line to, a caller that can wait writes it itself.
//!
//! The price of the task is the last moments before a hang: a line traced
//! just before a machine stops dead may still be in the ring. A panic pushes
//! out whatever is queued, the report with it.
//!
//! Every burst of lines is still a forced write, which is why the whole thing
//! is off unless `disklog=on` is given: a machine that merely has an area
//! prepared is not made to pay for it on every boot.
//!
//! WHERE IT WRITES, and why it will not eat a disk. The area is never guessed
//! and never searched for by "free space". A tool run under the host OS
//! (`scripts/disklog.py`) writes a header carrying a magic and a checksum to
//! the first sector of a partition set aside for this. At boot, given
//! `disklog=on`, the kernel reads the first sector of every block device it
//! has and writes only where that header is found intact. A disk that has not
//! been prepared is not written to, a partition holding anything else does
//! not carry the magic, and without `disklog=on` no disk is so much as read.

use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::disk::{self as block, Disk};
use kcore::cmd::Output;
use kcore::const_init::ConstInit;
use kcore::cpu;
use kcore::crc32::crc32_update;
use kcore::dma::DmaBuffer;
use kcore::once::Once;
use kcore::static_ring::Mailbox;
use kcore::sync::{Event, TryLock};
use kcore::task::{self, TaskHandle};
use kcore::trace;

/* ---- the on-disk header, first sector of the area ---- */

/// "NOSLOG1" read back as a little-endian u64. The layout below is what
/// `scripts/disklog.py` writes and reads.
const MAGIC: u64 = 0x0031_474F_4C53_4F4E;
const VERSION: u32 = 1;

/// 48 bytes: magic, version, sector size, area sectors, boot sequence, log
/// bytes, CRC, reserved.
const HEADER_SIZE: usize = 48;

/// Where the CRC sits, so the checksum never covers itself.
const CRC_OFFSET: usize = 40;

struct Header {
    sector_size: u32,
    area_sectors: u64,
    boot_seq: u64,
    log_bytes: u64,
}

impl Header {
    fn parse(sector: &[u8]) -> Option<(Self, u32)> {
        if sector.len() < HEADER_SIZE {
            return None;
        }
        let u32_at = |at: usize| u32::from_le_bytes(sector[at..at + 4].try_into().unwrap());
        let u64_at = |at: usize| u64::from_le_bytes(sector[at..at + 8].try_into().unwrap());

        if u64_at(0) != MAGIC || u32_at(8) != VERSION {
            return None;
        }
        let header = Self {
            sector_size: u32_at(12),
            area_sectors: u64_at(16),
            boot_seq: u64_at(24),
            log_bytes: u64_at(32),
        };
        Some((header, u32_at(CRC_OFFSET)))
    }

    fn write(&self, out: &mut [u8]) {
        out[..HEADER_SIZE].fill(0);
        out[0..8].copy_from_slice(&MAGIC.to_le_bytes());
        out[8..12].copy_from_slice(&VERSION.to_le_bytes());
        out[12..16].copy_from_slice(&self.sector_size.to_le_bytes());
        out[16..24].copy_from_slice(&self.area_sectors.to_le_bytes());
        out[24..32].copy_from_slice(&self.boot_seq.to_le_bytes());
        out[32..40].copy_from_slice(&self.log_bytes.to_le_bytes());
        let crc = crc32_update(0, &out[..CRC_OFFSET]);
        out[CRC_OFFSET..CRC_OFFSET + 4].copy_from_slice(&crc.to_le_bytes());
    }
}

/* ---- sizes ---- */

/// A line as the tracer makes it: `Tracer::Output` formats into 256 bytes,
/// and anything longer is cut to fit.
const MSG_SIZE: usize = 256;

/// Lines the writer has not taken yet. Before the area is found that is the
/// whole boot log, which is the part that matters and the part no other
/// channel can carry -- and a real machine prints far more of a boot than
/// QEMU does.
const MSG_COUNT: usize = 2048;

/// One page per transfer. Not a tuning choice: a DMA buffer has to be
/// physically contiguous, and one page is the largest block the allocator
/// guarantees that for. The flush loop makes as many trips as it needs.
const IO_BUF_SIZE: usize = 4096;

const MAX_SECTOR_SIZE: usize = 4096;

/// The writer's own staging: lines off the ring, and the tail of a sector
/// written only in part. Room for a transfer and a line over.
const PENDING_SIZE: usize = 2 * IO_BUF_SIZE;

/// The header's own sector. The area starts at the device's first sector,
/// which is where the host tool puts the header.
const AREA_START_SECTOR: u64 = 0;

/// What the disk log's claim on its device says to whoever is refused it.
const HOLDER: &core::ffi::CStr = c"the disk log";

/// How long `disklog` waits for a writer that is writing, before reporting
/// without its numbers.
const REPORT_TRIES: usize = 50;
const REPORT_WAIT_MS: u64 = 2;

/* ---- the lines waiting to be written ---- */

struct Line {
    text: [u8; MSG_SIZE],
    len: usize,
}

impl ConstInit for Line {
    const INIT: Self = Line { text: [0; MSG_SIZE], len: 0 };
}

/// From the tracer to the writer. A static with nothing to set up: this
/// whole channel has to work from the first line of the boot, long before
/// anything could run an initialiser.
static LINES: Mailbox<Line, MSG_COUNT> = Mailbox::new();

/* ---- where it writes, and how it is going ---- */

/// The device the area is on, as the block layer's handle; 0 for none.
static DEV: AtomicUsize = AtomicUsize::new(0);
static CLAIM: AtomicUsize = AtomicUsize::new(0);
static AREA_SECTORS: AtomicU64 = AtomicU64::new(0);
static SECTOR_SIZE: AtomicU32 = AtomicU32::new(0);
static BOOT_SEQ: AtomicU64 = AtomicU64::new(0);

/// An area was found and the writer is on it.
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Set once the log will not be written this boot: `disklog=on` not given,
/// no prepared area, no writer task, or `stop`. Nothing is queued after.
static OFF: AtomicBool = AtomicBool::new(false);

/// The tracer's drops: no free slot.
static DROPPED_LINES: AtomicU64 = AtomicU64::new(0);

/// The writer task's wake-up, and which task it is -- published before it
/// first runs. `log` wakes it through the event, and leaves out the lines
/// the task itself produces: an error on the write path traces, and writing
/// that line would fail and trace again.
static WAKE: Once<Event> = Once::new();
static WRITER_ID: AtomicUsize = AtomicUsize::new(0);
static WRITER_TASK: TryLock<Option<TaskHandle>> = TryLock::new(None);

/// What only the writer touches. One writer at a time -- the task, `setup`
/// catching up, `stop` finishing, or a caller with no scheduler to hand its
/// line to -- and nobody waits to be it: whoever finds the lock taken leaves
/// the work to whoever has it. The panic path goes round the lock, with the
/// rest of the machine already stopped.
struct Writer {
    /// Lines off the ring, and the tail of a sector written only in part.
    pending: [u8; PENDING_SIZE],
    pending_used: usize,
    /// Bytes of text on disk; always a whole number of sectors, so `pending`
    /// starts exactly where the next sector does.
    cursor: u64,
    full: bool,
    sector_writes: u64,
    write_failures: u64,
    /// The writer's drops: past the end of the area.
    dropped_bytes: u64,
    /// DMA targets, from the page allocator and not statics. A driver hands
    /// the buffer's physical address to the device, and only memory the
    /// allocator tracks has one it can find: a read into a `.bss` array comes
    /// back reporting success with the buffer untouched, which is a worse
    /// failure than an error would be and cost an afternoon to see.
    io: Option<DmaBuffer>,
    hdr: Option<DmaBuffer>,
}

static WRITER: TryLock<Writer> = TryLock::new(Writer {
    pending: [0; PENDING_SIZE],
    pending_used: 0,
    cursor: 0,
    full: false,
    sector_writes: 0,
    write_failures: 0,
    dropped_bytes: 0,
    io: None,
    hdr: None,
});

/// What the command line said: `Some(true)` for `disklog=on`, `Some(false)`
/// without it, and `None` while it has not been read yet.
fn wanted() -> Option<bool> {
    match unsafe { ffi::disklog::kernel_disklog_wanted() } {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

/* ---- the tracer's side ---- */

/// Append one line. Safe from any context, and never waits: the line is
/// queued for the writer. Nothing is queued once the command line has been
/// read without `disklog=on`, or once the log has been switched off.
pub fn log(line: &[u8]) {
    /* Switched off, or nothing to say. Read without a lock -- each flag is
     * set once, and a line that slips past the flip is only queued for a
     * writer that drops it. */
    if OFF.load(Ordering::Relaxed) || line.is_empty() {
        return;
    }

    /* Once the command line has been read, only for disklog=on. Before it
     * every line is kept: nobody knows yet whether it is wanted, and the
     * first lines are part of the boot the area is meant to hold. */
    if wanted() == Some(false) {
        return;
    }

    let writer_task = WRITER_ID.load(Ordering::Relaxed);
    if writer_task != 0 && task::current_id_or_none() == writer_task {
        return;
    }

    if !enqueue(line) {
        return;
    }

    if !ENABLED.load(Ordering::Relaxed) || trace::panic_active() {
        return;
    }

    /* Queued; now to whoever writes it. With the scheduler running that is
     * the task -- signalling an event is safe from any context, and the task
     * runs at its CPU's next scheduling point. Before there is a task,
     * `setup` is about to write the ring itself. */
    if cpu::preempt_is_on() {
        if let Some(wake) = WAKE.get() {
            wake.signal();
        }
        return;
    }

    /* No scheduler, so no task will ever run: a caller that may wait writes
     * the line itself, and one that may not -- interrupts off -- leaves it
     * queued for the next that can. */
    if cpu::preempt_can_block() {
        flush();
    }
}

/// A line into a free slot, and the slot onto the ready ring. No lock: this
/// is the tracer's side, and it runs wherever a trace does -- interrupt
/// handlers, code under a spinlock, the panic path. Preemption is held off
/// across it so that a task is never switched away between claiming a ready
/// cell and publishing it: the writer takes cells in order, and would wait
/// behind that one for as long as the task stayed away.
fn enqueue(line: &[u8]) -> bool {
    let held = cpu::preempt_disable_task();

    let queued = LINES.send(|slot| {
        let len = line.len().min(MSG_SIZE);
        slot.text[..len].copy_from_slice(&line[..len]);
        slot.len = len;
    });
    if !queued {
        DROPPED_LINES.fetch_add(1, Ordering::Relaxed);
    }

    cpu::preempt_enable_task(held);
    queued
}

/* ---- the writer's side ---- */

/// Lines off the ready ring into `pending`, as many as there is room for;
/// true if any came.
fn drain(w: &mut Writer) -> bool {
    let mut drained = false;

    while PENDING_SIZE - w.pending_used >= MSG_SIZE {
        let took = LINES.receive(|line| {
            let at = w.pending_used;
            w.pending[at..at + line.len].copy_from_slice(&line.text[..line.len]);
            w.pending_used += line.len;
        });
        if took.is_none() {
            break;
        }
        drained = true;
    }

    drained
}

/// Everything queued, onto the disk.
fn write_out(w: &mut Writer) {
    let dev = match Disk::from_handle(DEV.load(Ordering::Relaxed)) {
        Some(dev) => dev,
        None => return,
    };
    let sector_size = SECTOR_SIZE.load(Ordering::Relaxed) as usize;
    let area_sectors = AREA_SECTORS.load(Ordering::Relaxed);
    if sector_size == 0 {
        return;
    }

    loop {
        let drained = drain(w);

        if w.full {
            /* The area is used up. What is queued still has to leave the
             * ring, or its slots would never come back. */
            w.dropped_bytes += w.pending_used as u64;
            w.pending_used = 0;
            if !drained {
                break;
            }
            continue;
        }

        let n = w.pending_used.min(IO_BUF_SIZE);
        if n == 0 {
            break;
        }

        let sectors = n.div_ceil(sector_size);
        let first_sector = AREA_START_SECTOR + 1 + w.cursor / sector_size as u64;
        if first_sector + sectors as u64 > AREA_START_SECTOR + area_sectors {
            w.full = true;
            continue;
        }

        let bytes = sectors * sector_size;
        match w.io.as_mut() {
            Some(io) => {
                let buf = &mut io.as_mut_slice()[..bytes];
                buf[..n].copy_from_slice(&w.pending[..n]);
                buf[n..].fill(0);
            }
            None => return,
        }

        /* Forced to media: the point of this is to survive a machine that
         * stops immediately afterwards, and a write sitting in a cache does
         * not. */
        let io = w.io.as_ref().unwrap();
        if dev.write(first_sector, &io.as_slice()[..bytes], true).is_err() {
            w.write_failures += 1;
            break;
        }

        w.sector_writes += sectors as u64;

        /* Only whole sectors are retired. The tail of a partial one stays
         * staged and is written again next time, which is what makes the last
         * few lines before a hang appear on disk at all -- and a round that
         * retired nothing would only write the same partial sector again. */
        let retire = (n / sector_size) * sector_size;
        if retire == 0 {
            break;
        }

        w.cursor += retire as u64;
        w.pending_used -= retire;
        if w.pending_used != 0 {
            w.pending.copy_within(retire..retire + w.pending_used, 0);
        }

        /* Keep the length on disk in step with what is there. The reader can
         * find the end without it -- the area is zeroed and the text is not --
         * but a header that agrees is the difference between a tool that has
         * to guess and one that knows. */
        write_header(w, &dev);
    }
}

fn write_header(w: &mut Writer, dev: &Disk) -> bool {
    let sector_size = SECTOR_SIZE.load(Ordering::Relaxed) as usize;
    if sector_size == 0 {
        return false;
    }

    let header = Header {
        sector_size: sector_size as u32,
        area_sectors: AREA_SECTORS.load(Ordering::Relaxed),
        boot_seq: BOOT_SEQ.load(Ordering::Relaxed),
        log_bytes: w.cursor,
    };

    let hdr = match w.hdr.as_mut() {
        Some(hdr) => hdr,
        None => return false,
    };
    let buf = &mut hdr.as_mut_slice()[..sector_size];
    buf.fill(0);
    header.write(buf);

    if dev.write(AREA_START_SECTOR, &w.hdr.as_ref().unwrap().as_slice()[..sector_size], true).is_err()
    {
        w.write_failures += 1;
        return false;
    }
    true
}

/// Push what is queued to the device, from the writer's side.
fn flush() {
    if !ENABLED.load(Ordering::Relaxed) || DEV.load(Ordering::Relaxed) == 0 {
        return;
    }

    /* One writer at a time. Losing the race costs nothing: the winner drains
     * the ring, and a line queued behind its last look is the next wake's. */
    if let Some(mut writer) = WRITER.try_lock() {
        write_out(&mut writer);
    }
}

/// Push everything queued, from the panic path. Best effort by construction
/// -- the machine is going down either way.
pub fn panic_flush() {
    if !ENABLED.load(Ordering::Relaxed)
        || DEV.load(Ordering::Relaxed) == 0
        || OFF.load(Ordering::Relaxed)
    {
        return;
    }

    /* With the lock or without it: every other CPU has been sent the halting
     * IPI by now, and a lock one of them died holding -- the task stopped
     * mid-write, say -- must not keep the report off the disk. Interrupts are
     * off here, so the device write may not complete -- best effort, and the
     * console already has the report. A line whose producer was stopped
     * between claiming its cell and publishing it holds up the ring behind
     * it; what came before still goes. */
    match WRITER.try_lock() {
        Some(mut writer) => write_out(&mut writer),
        None => write_out(unsafe { WRITER.steal() }),
    }
}

/* ---- the writer task ---- */

fn run() {
    while !task::stopping() {
        flush();

        /* Nothing queued: out of the scheduler's way until `log` has a line.
         * The event counts its signals, so one that lands between the flush
         * above and the wait below is not lost -- the wait returns at once
         * and the next round takes the line. */
        match WAKE.get() {
            Some(wake) => wake.wait(),
            None => task::yield_to_runnable(),
        }
    }

    /* What arrived while it was being stopped. */
    flush();
}

fn start_task() -> bool {
    /* The event outlives the kernel: the task waits on it and `log` signals
     * it from anywhere. A second start finds the first one's. */
    if WAKE.get().is_none() {
        match Event::new() {
            Some(event) => { let _ = WAKE.set(event); }
            None => return false,
        }
    }

    let mut slot = match WRITER_TASK.try_lock() {
        Some(slot) => slot,
        None => return false,
    };

    /* Published the moment there is one (see WRITER_ID). A line the task
     * logs before that is queued like anyone's and written once, which is
     * harmless: what the id is for is a write that keeps failing, whose
     * every trace would otherwise be another write. */
    match task::spawn("disklog", run) {
        Some(handle) => {
            WRITER_ID.store(handle.id(), Ordering::Release);
            *slot = Some(handle);
            true
        }
        None => false,
    }
}

fn stop_task() {
    /* Unpublished first, so no new line reaches for a task on its way out. */
    WRITER_ID.store(0, Ordering::Release);

    let task = match WRITER_TASK.try_lock() {
        Some(mut slot) => slot.take(),
        None => None,
    };
    if let Some(task) = task {
        task.request_stop();
        if let Some(wake) = WAKE.get() {
            wake.signal();
        }
        /* Waits for it to have written what it had. */
        drop(task);
    }
}

/* ---- bring-up and tear-down ---- */

fn switch_off() {
    OFF.store(true, Ordering::Relaxed);
}

/// Given `disklog=on`, look for a prepared area on every registered block
/// device. Called once the block drivers are up. When one is found the boot
/// so far is written before this returns, and the writer task takes over.
/// False -- and the log is off for the rest of the boot, nothing more queued
/// -- when the parameter is not given or nothing is prepared, which is the
/// normal case and not an error.
pub fn setup() -> bool {
    /* Asked for, or nothing at all -- not a disk read, not a buffer. An area
     * outlives the debugging session it was prepared for, and finding one is
     * no reason for every later boot to pay a forced write per line. */
    if wanted() != Some(true) {
        switch_off();
        return false;
    }

    /* Nothing else is writing yet, and nothing will until this returns: the
     * area is not found, so `flush` does nothing and `log` only queues. */
    let mut writer = match WRITER.try_lock() {
        Some(writer) => writer,
        None => {
            switch_off();
            return false;
        }
    };
    let w = &mut *writer;
    if w.io.is_none() {
        w.io = DmaBuffer::new(IO_BUF_SIZE / 4096);
        w.hdr = DmaBuffer::new(MAX_SECTOR_SIZE / 4096);
        if w.io.is_none() || w.hdr.is_none() {
            trace!(0, "DiskLog: no memory for the transfer buffers");
            switch_off();
            return false;
        }
    }

    for index in 0..block::count() {
        let dev = match block::at(index) {
            Some(dev) => dev,
            None => continue,
        };

        /* Reading here is safe: this runs once, in the boot task with
         * interrupts on. */
        let header = match read_header(w, &dev) {
            Some(header) => header,
            None => continue,
        };

        /* The area is the disk log's from here on: a mount of the device, or
         * of the disk it is on, or a module writing to it direct is refused */
        let claim = match block::claim_as(dev.handle(), HOLDER) {
            Ok(claim) => claim,
            Err(held_by) => {
                trace!(0, "DiskLog: {} is in use by {}, not writing to it",
                    dev.name(), held_by);
                continue;
            }
        };

        DEV.store(dev.handle(), Ordering::Relaxed);
        CLAIM.store(claim, Ordering::Relaxed);
        AREA_SECTORS.store(header.area_sectors, Ordering::Relaxed);
        SECTOR_SIZE.store(header.sector_size, Ordering::Relaxed);
        BOOT_SEQ.store(header.boot_seq + 1, Ordering::Relaxed);
        w.cursor = 0;
        w.full = false;
        ENABLED.store(true, Ordering::Release);

        /* The header goes down before any text, so a machine that stops on
         * the very next line still leaves a readable area rather than the
         * previous boot's text under a stale length. */
        if !write_header(w, &dev) {
            ENABLED.store(false, Ordering::Relaxed);
            switch_off();
            DEV.store(0, Ordering::Relaxed);
            CLAIM.store(0, Ordering::Relaxed);
            block::release(claim);
            return false;
        }

        trace!(0, "DiskLog: {}, boot {}, {} sectors of {} bytes",
            dev.name(), header.boot_seq + 1,
            header.area_sectors, header.sector_size);

        /* The boot so far -- the whole ring, the line above with it -- goes
         * down here, in this context and before setup returns: a machine that
         * stops right after still leaves all of it. */
        write_out(w);
        drop(writer);

        if !start_task() {
            trace!(0, "DiskLog: no writer task, the log on disk stops here");
            flush();
            switch_off();
            block::release(CLAIM.swap(0, Ordering::Relaxed));
            return false;
        }

        return true;
    }

    /* Asked for and not found: worth a line, since whoever asked is about to
     * go looking for a log that was never written. */
    switch_off();
    trace!(0, "DiskLog: disklog=on, but no prepared area on any disk");
    false
}

/// Does the device carry a prepared area? The header has to be intact, name
/// the device's own sector size, and describe an area that fits the device
/// and holds more than its own header.
fn read_header(w: &mut Writer, dev: &Disk) -> Option<Header> {
    let sector_size = dev.sector_size() as usize;
    if sector_size < HEADER_SIZE || sector_size > MAX_SECTOR_SIZE {
        return None;
    }

    let io = w.io.as_mut()?;
    if dev.read(0, &mut io.as_mut_slice()[..sector_size]).is_err() {
        return None;
    }

    let (header, crc) = Header::parse(&w.io.as_ref()?.as_slice()[..sector_size])?;
    if header.sector_size != sector_size as u32 {
        return None;
    }
    if header.area_sectors < 2 || header.area_sectors > dev.sectors() {
        return None;
    }

    /* The checksum, over everything before itself. */
    let mut fresh = [0u8; HEADER_SIZE];
    Header {
        sector_size: header.sector_size,
        area_sectors: header.area_sectors,
        boot_seq: header.boot_seq,
        log_bytes: header.log_bytes,
    }
    .write(&mut fresh);
    if crc32_update(0, &fresh[..CRC_OFFSET]) != crc {
        return None;
    }

    Some(header)
}

/// On the way down, before the soft IRQs stop: the writer finishes what is
/// queued and exits, and the log switches off -- after `SoftIrq::Stop` a
/// write through a virtio disk, which completes by soft IRQ, would wait for
/// ever.
pub fn stop() {
    if !ENABLED.load(Ordering::Relaxed) || OFF.load(Ordering::Relaxed) {
        return;
    }

    /* The task writes what is queued on its way out, and what arrives after
     * is written here. Then off: nothing may wait on a block write once the
     * soft IRQs are gone. */
    stop_task();
    flush();
    switch_off();

    /* Nothing writes the area again: it goes back to whoever wants it */
    block::release(CLAIM.swap(0, Ordering::Relaxed));
    DEV.store(0, Ordering::Relaxed);
}

/* ---- what `disklog` shows ---- */

pub fn dump(_args: &str, out: &mut Output) {
    if !ENABLED.load(Ordering::Relaxed) {
        if wanted() != Some(true) {
            let _ = writeln!(out, "disklog: off -- boot with disklog=on to write the \
log to a prepared area");
            return;
        }

        let _ = writeln!(out, "disklog: no prepared area found");
        let _ = writeln!(out, "  {} lines queued, {} dropped",
            LINES.waiting(), DROPPED_LINES.load(Ordering::Relaxed));
        return;
    }

    let dev_name = Disk::from_handle(DEV.load(Ordering::Relaxed)).map_or("?", |dev| dev.name());

    let _ = writeln!(out, "disklog: {}, boot {}, {} sectors of {} bytes",
        dev_name, BOOT_SEQ.load(Ordering::Relaxed),
        AREA_SECTORS.load(Ordering::Relaxed), SECTOR_SIZE.load(Ordering::Relaxed));

    /* The writer's own numbers are the writer's while it writes. A write is
     * milliseconds, so it is waited for a little -- and no longer: a report
     * that says the writer is busy is still a report. */
    let mut writer = WRITER.try_lock();
    for _ in 0..REPORT_TRIES {
        if writer.is_some() {
            break;
        }
        task::sleep_ms(REPORT_WAIT_MS);
        writer = WRITER.try_lock();
    }

    match writer {
        Some(w) => {
            let _ = writeln!(out, "  on disk {} bytes, staged {}, queued {}, sector writes {}",
                w.cursor, w.pending_used, LINES.waiting(), w.sector_writes);
            let _ = writeln!(out, "  failures {}, dropped {} lines and {} bytes, full {}, off {}",
                w.write_failures, DROPPED_LINES.load(Ordering::Relaxed), w.dropped_bytes,
                w.full as u32, OFF.load(Ordering::Relaxed) as u32);
        }
        None => {
            let _ = writeln!(out, "  the writer is busy; queued {}, dropped {} lines, off {}",
                LINES.waiting(), DROPPED_LINES.load(Ordering::Relaxed),
                OFF.load(Ordering::Relaxed) as u32);
        }
    }
}

/* ---- what the kernel calls ---- */

/// One line from the tracer, NUL-terminated as it formatted it.
///
/// # Safety
/// `line` points at a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn rust_disklog_log(line: *const u8) {
    if line.is_null() {
        return;
    }
    /* Bytes, whatever they spell: a line that is not UTF-8 still belongs in
     * the log. */
    log(unsafe { core::ffi::CStr::from_ptr(line.cast()) }.to_bytes());
}

#[no_mangle]
pub extern "C" fn rust_disklog_setup() -> i32 {
    setup() as i32
}

#[no_mangle]
pub extern "C" fn rust_disklog_stop() {
    stop();
}

#[no_mangle]
pub extern "C" fn rust_disklog_panic_flush() {
    panic_flush();
}
