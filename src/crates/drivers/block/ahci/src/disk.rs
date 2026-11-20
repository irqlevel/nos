//! A disk on a port, and its commands: up to 31 queued at once (NCQ), each in
//! a slot of the port's command list with a table of its own; ended by the
//! HBA's interrupt (`reap`, from the ISR) or, before there are interrupts to
//! wait on -- at boot, at the halt -- by whoever waits, polling the port. A
//! task waiting on its command gives its CPU away for a moment on a disk
//! that answers in microseconds, and then sleeps on the slot's event, which
//! the command's end signals: a disk that spins answers in milliseconds, and
//! a waiter that spun held a CPU for as long as the disk took. The
//! disk's own commands -- a flush, a standby, the NCQ error log -- go on one
//! more slot with the queue drained (`own_command`). A command that fails,
//! times out or loses its link has the port taken back and every command it
//! had accounted for (`recover`): the one that failed failed, the rest sent
//! again, the disk given up on where that is all there is left to do.

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

use ataproto::cmd;
use ataproto::identify::{Flush, Identify, Rotation};
use ataproto::{ncq_error, NcqError, LOG_NCQ_ERROR, LOG_PAGE_BYTES, MAX_UNFINISHED};
use block::Piece;
use kcore::block::{BlockIo, SubmitError, IO_FLUSH, IO_READ, IO_WRITE};
use kcore::consts::PAGE_SIZE;
use kcore::dma::{self, DmaBuffer};
use kcore::klog;
use kcore::sync::{Event, SpinLock, TryLock};
use kcore::time::{boot_time_ns, Deadline, Duration};

use crate::port::{relax, PortRegs};
use crate::regs::*;
use crate::Hba;

const NS_PER_MS: u64 = 1_000_000;

/// A slot's outcome, for the task that waits on it.
const PENDING: u8 = 0;
const DONE_OK: u8 = 1;
const DONE_FAILED: u8 = 2;

/// What an asynchronous request's caller is told of one that failed.
const STATUS_FAILED: i32 = 1;

/// Why a command did not go through.
#[derive(Clone, Copy)]
pub(crate) enum Fault {
    /// The HBA still had it after its time: PxIS, PxTFD, PxCI then.
    Timeout { is: u32, tfd: u32, ci: u32 },
    /// The HBA or the device said it failed: PxIS and PxTFD.
    Device { is: u32, tfd: u32 },
    /// Done, but fewer bytes moved than were asked for.
    Short { moved: u32, asked: usize },
    /// The link went from under it (`IS_LINK`): PxIS and PxSSTS then.
    Link { is: u32, ssts: u32 },
    /// A buffer the HBA cannot take: no physical address, an odd one, past
    /// 32 bits on an HBA without 64-bit addressing, more pieces than a PRDT.
    Buffer,
}

/// PxTFD's status and error, as a log line says them.
pub(crate) fn status_error(tfd: u32) -> String {
    alloc::format!("status {:#x} error {:#x}", tfd & TFD_STATUS_MASK, (tfd >> TFD_ERROR_SHIFT) & TFD_STATUS_MASK)
}

impl Fault {
    /// What the log says of it.
    pub(crate) fn describe(self) -> String {
        match self {
            Fault::Timeout { is, tfd, ci } => {
                alloc::format!("timed out, is {:#x} ci {:#x}, {}", is, ci, status_error(tfd))
            }
            Fault::Device { is, tfd } => alloc::format!("failed, is {:#x}, {}", is, status_error(tfd)),
            Fault::Short { moved, asked } => alloc::format!("moved {} of the {} bytes asked", moved, asked),
            Fault::Link { is, ssts } => alloc::format!("lost its link, is {:#x} ssts {:#x}", is, ssts),
            Fault::Buffer => String::from("a buffer the HBA cannot take"),
        }
    }
}

/// Whom a command's end is for.
#[derive(Clone, Copy)]
enum Waiter {
    /// A task, polling `outcome` for the slot it holds and giving it back.
    Task,
    /// An asynchronous request's caller, told from wherever the end is seen
    /// -- the ISR, mostly -- with the slot given back.
    Call { done: extern "C" fn(*mut u8, i32), ctx: usize },
}

#[derive(Clone, Copy)]
struct Slot {
    waiter: Option<Waiter>,
    /// What it is, for the log
    what: &'static str,
    lba: u64,
    count: u32,
    /// NCQ: ended by its PxSACT bit, not by PxCI alone
    queued: bool,
    /// What a command that is not queued has to move (PRDBC); 0 for none
    bytes: u32,
    timeout_ms: u64,
    /// How many times it has gone to the disk
    tries: u8,
}

impl Slot {
    const EMPTY: Slot =
        Slot { waiter: None, what: "", lba: 0, count: 0, queued: false, bytes: 0, timeout_ms: 0, tries: 0 };
}

/// Each bit of a mask, lowest first.
pub(crate) fn bits(mut mask: u32) -> impl Iterator<Item = usize> {
    core::iter::from_fn(move || {
        if mask == 0 {
            return None;
        }
        let t = mask.trailing_zeros() as usize;
        mask &= mask - 1;
        Some(t)
    })
}

/// Asynchronous callers to tell, once the lock is let go of: their own
/// callbacks may submit again the moment they hear. And the slots whose
/// task may sleep on its command's end, woken then too.
struct Calls {
    list: [Option<(extern "C" fn(*mut u8, i32), usize, i32)>; MAX_SLOTS],
    n: usize,
    wake: u32,
}

impl Calls {
    fn new() -> Calls {
        Calls { list: [None; MAX_SLOTS], n: 0, wake: 0 }
    }

    fn push(&mut self, done: extern "C" fn(*mut u8, i32), ctx: usize, status: i32) {
        /* A slot's end comes once, and there are MAX_SLOTS of them */
        if let Some(entry) = self.list.get_mut(self.n) {
            *entry = Some((done, ctx, status));
            self.n += 1;
        }
    }

    /// `ended`: the disk's, a slot's each (`AhciDisk::ended`).
    fn run(&mut self, ended: &[Event]) {
        for t in bits(core::mem::take(&mut self.wake)) {
            if let Some(event) = ended.get(t) {
                event.signal();
            }
        }
        for entry in self.list[..self.n].iter_mut() {
            if let Some((done, ctx, status)) = entry.take() {
                done(ctx as *mut u8, status);
            }
        }
        self.n = 0;
    }
}

/// What the disk's lock owns: the port's memory and every command it has.
pub(crate) struct Queue {
    /// The command list, the received FIS area, the scratch sector
    mem: DmaBuffer,
    /// A command table a slot
    tables: DmaBuffer,
    slots: [Slot; MAX_SLOTS],
    /// Queued slots nobody holds
    free: u32,
    /// Issued -- PxCI set, and for NCQ PxSACT -- and not seen ended
    issued: u32,
    /// Built, the doorbell owed
    owed: u32,
    /// One of the disk's own commands has the port, or waits for the queue
    /// to drain to have it: nothing queued is issued meanwhile
    exclusive: bool,
    /// An asynchronous flush's caller, its flush waiting for the queue to
    /// drain: issued by whoever sees it drained (`start_flush`)
    flush_call: Option<(extern "C" fn(*mut u8, i32), usize)>,
    /// The error handler has the port: nothing is issued or reaped by
    /// anyone else, and the port interrupts for nothing
    recovering: bool,
    /// What the ISR saw of an error or a change of the link, for the error
    /// handler: PxIS's bits and PxTFD
    seen_is: u32,
    seen_tfd: u32,
    /// Commands in a row that did not finish
    unfinished: u32,
    /// Given up on: every request refused
    failed: bool,
}

impl Queue {
    /// A port's memory, `slots` command tables of it: None when it is not
    /// there to be had, or is past what the HBA addresses.
    pub(crate) fn new(slots: usize, s64a: bool) -> Option<Queue> {
        let mem = DmaBuffer::new(1)?;
        let tables = DmaBuffer::new((slots * TABLE_BYTES).div_ceil(PAGE_SIZE))?;
        let reach = |b: &DmaBuffer, len: usize| b.phys().checked_add(len as u64 - 1).is_some_and(|end| end >> 32 == 0);
        if !s64a && (!reach(&mem, PAGE_SIZE) || !reach(&tables, slots * TABLE_BYTES)) {
            return None;
        }
        Some(Queue {
            mem,
            tables,
            slots: [Slot::EMPTY; MAX_SLOTS],
            free: 0,
            issued: 0,
            owed: 0,
            exclusive: false,
            flush_call: None,
            recovering: false,
            seen_is: 0,
            seen_tfd: 0,
            unfinished: 0,
            failed: false,
        })
    }

    pub(crate) fn command_list(&self) -> u64 {
        self.mem.phys() + CL_OFFSET as u64
    }

    pub(crate) fn fis_area(&self) -> u64 {
        self.mem.phys() + FIS_OFFSET as u64
    }

    fn scratch(&self) -> u64 {
        self.mem.phys() + SCRATCH_OFFSET as u64
    }

    /// One PRD entry of slot `slot`: `len` bytes at `phys`.
    fn put_prd(&mut self, slot: usize, index: usize, phys: u64, len: usize, s64a: bool) -> Result<(), Fault> {
        let end = phys.checked_add(len as u64).ok_or(Fault::Buffer)?;
        if index >= PRDS_PER_SLOT || len == 0 || len % 2 != 0 || len > PRD_MAX_BYTES || phys & 1 != 0
            || (!s64a && (end - 1) >> 32 != 0)
        {
            return Err(Fault::Buffer);
        }
        let at = slot * TABLE_BYTES + TABLE_PRDT + index * PRD_BYTES;
        let entry = self.tables.bytes_mut(at, PRD_BYTES).ok_or(Fault::Buffer)?;
        entry[0..4].copy_from_slice(&(phys as u32).to_le_bytes());
        entry[4..8].copy_from_slice(&((phys >> 32) as u32).to_le_bytes());
        entry[8..12].copy_from_slice(&0u32.to_le_bytes());
        entry[12..16].copy_from_slice(&(((len - 1) as u32) & PRD_DBC_MASK).to_le_bytes());
        Ok(())
    }

    /// Slot `slot`'s PRDT over `len` bytes at the kernel's `addr`, its pages
    /// found by their physical addresses and runs of them that are
    /// contiguous made one entry: how many entries.
    fn prdt_virt(&mut self, slot: usize, addr: usize, len: usize, s64a: bool) -> Result<usize, Fault> {
        let mut entries = 0usize;
        let mut run: Option<(u64, usize)> = None;
        let mut done = 0usize;
        while done < len {
            let at = addr.checked_add(done).ok_or(Fault::Buffer)?;
            let piece = (PAGE_SIZE - at % PAGE_SIZE).min(len - done);
            let phys = dma::virt_to_phys(at as *const u8);
            if phys == 0 {
                return Err(Fault::Buffer);
            }
            run = match run {
                Some((start, bytes)) if start.checked_add(bytes as u64) == Some(phys)
                    && bytes + piece <= PRD_MAX_BYTES => Some((start, bytes + piece)),
                Some((start, bytes)) => {
                    self.put_prd(slot, entries, start, bytes, s64a)?;
                    entries += 1;
                    Some((phys, piece))
                }
                None => Some((phys, piece)),
            };
            done += piece;
        }
        if let Some((start, bytes)) = run {
            self.put_prd(slot, entries, start, bytes, s64a)?;
            entries += 1;
        }
        Ok(entries)
    }

    /// Slot `slot`'s PRDT over `len` physically contiguous bytes at `phys`.
    fn prdt_contiguous(&mut self, slot: usize, phys: u64, len: usize, s64a: bool) -> Result<usize, Fault> {
        let mut entries = 0usize;
        let mut done = 0usize;
        while done < len {
            let n = (len - done).min(PRD_MAX_BYTES);
            self.put_prd(slot, entries, phys + done as u64, n, s64a)?;
            entries += 1;
            done += n;
        }
        Ok(entries)
    }

    /// Slot `slot`'s PRDT for the one command that takes `pieces` from the
    /// first on, each `len` bytes at `phys + at` of a batch's buffer -- for as
    /// long as they follow one another on the disk and the command has room:
    /// at most `limit` bytes and PRDS_PER_SLOT entries, pieces next to each
    /// other in memory one entry. How many pieces it takes, their bytes, and
    /// the entries.
    fn prdt_pieces(&mut self, slot: usize, phys: u64, pieces: &[Piece], sector_size: usize, limit: usize,
                   s64a: bool) -> Result<(usize, usize, usize), Fault>
    {
        let mut taken = 0usize;
        let mut bytes = 0usize;
        let mut entries = 0usize;
        let mut run: Option<(u64, usize)> = None;
        let mut next_sector = pieces.first().map_or(0, |p| p.sector);
        for p in pieces {
            if p.sector != next_sector || p.len == 0 || p.len % sector_size != 0
                || bytes.checked_add(p.len).is_none_or(|total| total > limit)
            {
                break;
            }
            let at = phys.checked_add(p.at as u64).ok_or(Fault::Buffer)?;
            run = match run {
                Some((start, len)) if start.checked_add(len as u64) == Some(at) && len + p.len <= PRD_MAX_BYTES => {
                    Some((start, len + p.len))
                }
                Some((start, len)) => {
                    /* Room for the run closed here and the one this piece
                     * opens, or the command ends before it */
                    if entries + 2 > PRDS_PER_SLOT {
                        break;
                    }
                    self.put_prd(slot, entries, start, len, s64a)?;
                    entries += 1;
                    Some((at, p.len))
                }
                None => Some((at, p.len)),
            };
            bytes += p.len;
            next_sector = p.sector.checked_add((p.len / sector_size) as u64).ok_or(Fault::Buffer)?;
            taken += 1;
        }
        if let Some((start, len)) = run {
            self.put_prd(slot, entries, start, len, s64a)?;
            entries += 1;
        }
        if taken == 0 {
            return Err(Fault::Buffer);
        }
        Ok((taken, bytes, entries))
    }

    /// Slot `slot`'s command FIS and header, over `prds` PRD entries
    /// written already.
    fn command(&mut self, slot: usize, fis: &[u8; FIS_H2D_BYTES], prds: usize, write: bool) -> Result<(), Fault> {
        let table = slot * TABLE_BYTES;
        {
            let cfis = self.tables.bytes_mut(table, CFIS_BYTES).ok_or(Fault::Buffer)?;
            cfis.fill(0);
            cfis[..FIS_H2D_BYTES].copy_from_slice(fis);
        }
        let ctba = self.tables.phys() + table as u64;
        let header = self.mem.bytes_mut(CL_OFFSET + slot * HEADER_BYTES, HEADER_BYTES).ok_or(Fault::Buffer)?;
        header.fill(0);
        let flags = HDR_CFL_H2D | if write { HDR_WRITE } else { 0 } | ((prds as u32) << HDR_PRDTL_SHIFT);
        header[0..4].copy_from_slice(&flags.to_le_bytes());
        header[8..12].copy_from_slice(&(ctba as u32).to_le_bytes());
        header[12..16].copy_from_slice(&((ctba >> 32) as u32).to_le_bytes());
        Ok(())
    }

    /// Slot `slot`'s byte count (PRDBC) back to none, for it to go again.
    fn reset_moved(&mut self, slot: usize) {
        if let Some(count) = self.mem.bytes_mut(CL_OFFSET + slot * HEADER_BYTES + 4, 4) {
            count.fill(0);
        }
    }

    /// What the HBA says slot `slot`'s command moved (PRDBC).
    fn moved(&self, slot: usize) -> u32 {
        self.mem.bytes(CL_OFFSET + slot * HEADER_BYTES + 4, 4).map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Whether slot `slot`'s command, ended, moved what it was to.
    fn moved_all(&self, slot: usize) -> bool {
        let s = &self.slots[slot];
        s.queued || s.bytes == 0 || self.moved(slot) == s.bytes
    }

    /// The scratch sector, read once a command has filled it.
    fn scratch_bytes(&self) -> Option<&[u8]> {
        self.mem.bytes(SCRATCH_OFFSET, SCRATCH_BYTES)
    }
}

/* ---- the FIS a command goes as ---- */

/// A read's, a write's or a command's that moves nothing, not queued: LBA48
/// where the disk has it, a 28-bit command's address and count otherwise.
/// `count` is at most 65536 for the one and 256 for the other, which the
/// callers keep to.
pub(crate) fn fis_dma(command: u8, lba: u64, count: u64, lba48: bool) -> [u8; FIS_H2D_BYTES] {
    let mut fis = [0u8; FIS_H2D_BYTES];
    fis[0] = FIS_TYPE_H2D;
    fis[1] = FIS_H2D_COMMAND;
    fis[2] = command;
    fis[4] = lba as u8;
    fis[5] = (lba >> 8) as u8;
    fis[6] = (lba >> 16) as u8;
    if lba48 {
        fis[7] = FIS_DEVICE_LBA;
        fis[8] = (lba >> 24) as u8;
        fis[9] = (lba >> 32) as u8;
        fis[10] = (lba >> 40) as u8;
        fis[13] = (count >> 8) as u8;
    } else {
        fis[7] = FIS_DEVICE_LBA | ((lba >> 24) & 0x0F) as u8;
    }
    /* 65536 and 256 are 0 here, which is what they mean */
    fis[12] = count as u8;
    fis
}

/// An NCQ read's or write's: the count in the features registers, the tag in
/// the count register's top five bits, a write's FUA in the device
/// register's top bit.
fn fis_fpdma(write: bool, fua: bool, lba: u64, count: u64, tag: usize) -> [u8; FIS_H2D_BYTES] {
    let mut fis = [0u8; FIS_H2D_BYTES];
    fis[0] = FIS_TYPE_H2D;
    fis[1] = FIS_H2D_COMMAND;
    fis[2] = if write { cmd::WRITE_FPDMA_QUEUED } else { cmd::READ_FPDMA_QUEUED };
    fis[3] = count as u8;
    fis[4] = lba as u8;
    fis[5] = (lba >> 8) as u8;
    fis[6] = (lba >> 16) as u8;
    fis[7] = FIS_DEVICE_LBA | if fua { FIS_DEVICE_FUA } else { 0 };
    fis[8] = (lba >> 24) as u8;
    fis[9] = (lba >> 32) as u8;
    fis[10] = (lba >> 40) as u8;
    fis[11] = (count >> 8) as u8;
    fis[12] = (tag as u8) << FIS_TAG_SHIFT;
    fis
}

/// READ LOG EXT of one sector of log `log`.
fn fis_read_log(log: u8) -> [u8; FIS_H2D_BYTES] {
    let mut fis = fis_dma(cmd::READ_LOG_EXT, 0, 1, true);
    fis[4] = log;
    fis
}

/* ---- a disk ---- */

pub(crate) struct AhciDisk {
    pub(crate) hba: &'static Hba,
    pub(crate) port: PortRegs,
    queue: SpinLock<Queue>,
    /// Per slot, for a task that waits on it: PENDING, DONE_OK, DONE_FAILED
    outcome: [AtomicU8; MAX_SLOTS],
    /// Per slot, signalled once its outcome is set, for the task that sleeps
    /// on it (`wait_slot`): the slot's holder, the one waiter an event takes
    ended: Vec<Event>,
    /// How long a waiter looks before it sleeps: SPIN_SOLID_NS, or none for
    /// a disk that spins
    spin_ns: u64,
    /// The nap of a wait no slot's end signals, once past `spin_ns`
    nap_ns: u64,
    /// Per slot, when its command is out past its time (boot ns); 0 while
    /// it is not out
    deadline: [AtomicU64; MAX_SLOTS],
    /// An error or a link change seen: the error handler is wanted
    attention: AtomicBool,
    /// Whoever takes the port back after a failure -- the error-handling
    /// task, or a waiter: one at a time, and nobody waits for it
    eh: TryLock<()>,
    /// The queued slots: 0..depth
    depth: u32,
    /// The slot of the disk's own commands
    own: usize,
    ncq: bool,
    /// For the log: the HBA's address and the port's number on it
    pub(crate) at: (u16, u16, u16, u32),
    pub(crate) id: Identify,
    pub(crate) identity: Option<String>,
    pub(crate) name: [u8; 3],
}

impl AhciDisk {
    pub(crate) fn new(hba: &'static Hba, port: PortRegs, queue: Queue, slots: usize, at: (u16, u16, u16, u32),
                      id: Identify, name: [u8; 3]) -> Option<AhciDisk>
    {
        /* NCQ where the HBA and the disk both have it and there is a slot to
         * spare for the disk's own commands: as deep as the shallower */
        let ncq_depth = match id.ncq_depth {
            Some(depth) if hba.cap & CAP_SNCQ != 0 && slots >= 3 => usize::from(depth).min(slots - 1),
            _ => 1,
        };
        let ncq = ncq_depth >= 2;
        let depth = ncq_depth as u32;
        let own = if slots > ncq_depth { ncq_depth } else { 0 };
        let mut queue = queue;
        queue.free = if depth >= 32 { u32::MAX } else { (1u32 << depth) - 1 };
        let queue = SpinLock::new(queue)?;
        let identity = ataproto::identify::identity_word(&id.model, &id.serial);
        let mut ended = Vec::new();
        ended.try_reserve_exact(MAX_SLOTS).ok()?;
        for _ in 0..MAX_SLOTS {
            ended.push(Event::new()?);
        }
        let (spin_ns, nap_ns) = match id.rotation {
            Rotation::Rpm(_) => (0, NAP_SPINNING_NS),
            Rotation::Solid | Rotation::NotReported => (SPIN_SOLID_NS, NAP_SOLID_NS),
        };
        Some(AhciDisk {
            hba,
            port,
            queue,
            outcome: [const { AtomicU8::new(PENDING) }; MAX_SLOTS],
            ended,
            spin_ns,
            nap_ns,
            deadline: [const { AtomicU64::new(0) }; MAX_SLOTS],
            attention: AtomicBool::new(false),
            eh: TryLock::new(()),
            depth,
            own,
            ncq,
            at,
            id,
            identity,
            name,
        })
    }

    pub(crate) fn name(&self) -> &str {
        core::str::from_utf8(&self.name).unwrap_or("ad?")
    }

    /// How many commands it has out at once at most: NCQ's depth, or one.
    pub(crate) fn depth(&self) -> u32 {
        self.depth
    }

    fn queued_mask(&self) -> u32 {
        if self.depth >= 32 { u32::MAX } else { (1u32 << self.depth) - 1 }
    }

    fn s64a(&self) -> bool {
        self.port.cap & CAP_S64A != 0
    }

    pub(crate) fn failed(&self) -> bool {
        self.queue.lock().failed
    }

    /// Whether `count` sectors from `lba` are inside the disk -- and inside
    /// what 28 bits reach on one without 48-bit commands. The table checked
    /// what it hands down; this is the driver's own check on the address it
    /// is about to give the device.
    fn within(&self, lba: u64, count: u64) -> bool {
        let limit = if self.id.lba48 { self.id.sectors } else { self.id.sectors.min(LBA28_LIMIT) };
        count != 0 && lba.checked_add(count).is_some_and(|end| end <= limit)
    }

    /// The most bytes one command moves on this disk: MAX_BYTES_PER_COMMAND,
    /// or what its count register reaches, whole sectors.
    fn per_command(&self) -> usize {
        let size = self.id.sector_size as usize;
        let count = if self.id.lba48 { LBA48_COUNT_MAX } else { LBA28_COUNT_MAX } as usize;
        (MAX_BYTES_PER_COMMAND / size).min(count).max(1) * size
    }

    /// The FIS of a read or a write of `count` sectors at `lba` on slot
    /// `slot`: queued where the disk has NCQ; with `fua`, a write the disk
    /// has made durable when it ends -- which only a disk with FUA is asked.
    fn data_fis(&self, write: bool, fua: bool, lba: u64, count: u64, slot: usize) -> [u8; FIS_H2D_BYTES] {
        if self.ncq {
            return fis_fpdma(write, fua, lba, count, slot);
        }
        let command = match (write, fua, self.id.lba48) {
            (false, _, true) => cmd::READ_DMA_EXT,
            (true, true, true) => cmd::WRITE_DMA_FUA_EXT,
            (true, false, true) => cmd::WRITE_DMA_EXT,
            (false, _, false) => cmd::READ_DMA,
            (true, _, false) => cmd::WRITE_DMA,
        };
        fis_dma(command, lba, count, self.id.lba48)
    }

    /// A data command's slot, filled for the log and for going again.
    fn data_slot(&self, waiter: Waiter, write: bool, lba: u64, count: u64, bytes: usize) -> Slot {
        Slot {
            waiter: Some(waiter),
            what: if write { "write" } else { "read" },
            lba,
            count: count as u32,
            queued: self.ncq,
            bytes: if self.ncq { 0 } else { bytes as u32 },
            timeout_ms: COMMAND_MS,
            tries: 1,
        }
    }

    /// Whether a waiter can leave its wait to the interrupt: the HBA's comes,
    /// the boot is past where one can be taken, and this CPU takes them now
    /// -- not at the halt, where they are off for good.
    fn interrupt_mode(&self) -> bool {
        self.hba.interrupts_work() && block::interrupts_started() && kcore::cpu::interrupts_enabled()
    }

    /* ---- issuing and ending ---- */

    /// The doorbell for what is owed: each command's time from now, PxSACT
    /// for the queued ones, then PxCI.
    fn ring(&self, q: &mut Queue) {
        if q.owed == 0 || q.recovering || q.failed {
            return;
        }
        let now = boot_time_ns();
        let mut queued = 0u32;
        for t in bits(q.owed) {
            self.deadline[t].store(now.saturating_add(q.slots[t].timeout_ms * NS_PER_MS), Ordering::Relaxed);
            if q.slots[t].queued {
                queued |= 1 << t;
            }
        }
        /* The headers, tables and PRDTs in memory before the HBA fetches them */
        kcore::barrier::dma_wmb();
        if queued != 0 {
            self.port.write(PX_SACT, queued);
        }
        self.port.write(PX_CI, q.owed);
        q.issued |= q.owed;
        q.owed = 0;
    }

    /// Slot `t`'s command ended, `ok` or not: told to its task, or to its
    /// caller -- by `calls`, out of the lock -- with the slot given back.
    fn finish(&self, q: &mut Queue, t: usize, ok: bool, calls: &mut Calls) {
        self.deadline[t].store(0, Ordering::Relaxed);
        match q.slots[t].waiter.take() {
            Some(Waiter::Task) => {
                self.outcome[t].store(if ok { DONE_OK } else { DONE_FAILED }, Ordering::Release);
                calls.wake |= 1 << t;
            }
            Some(Waiter::Call { done, ctx }) => {
                calls.push(done, ctx, if ok { 0 } else { STATUS_FAILED });
                q.slots[t] = Slot::EMPTY;
                if self.queued_mask() & (1 << t) != 0 {
                    q.free |= 1 << t;
                } else {
                    /* The own slot's caller: the asynchronous flush's, done */
                    q.exclusive = false;
                }
            }
            None => {}
        }
    }

    /// The asynchronous flush waiting for the queue, issued on the disk's own
    /// slot once nothing else is out: by whoever sees the queue drained.
    fn start_flush(&self, q: &mut Queue) {
        if q.flush_call.is_none() || q.issued != 0 || q.owed != 0 || q.recovering || q.failed {
            return;
        }
        let Some((done, ctx)) = q.flush_call.take() else { return };
        let t = self.own;
        let command = if self.id.flush == Flush::FlushExt { cmd::FLUSH_CACHE_EXT } else { cmd::FLUSH_CACHE };
        q.slots[t] = Slot { waiter: Some(Waiter::Call { done, ctx }), what: "flush", queued: false,
                            timeout_ms: FLUSH_MS, tries: MAX_TRIES, ..Slot::EMPTY };
        if q.command(t, &fis_dma(command, 0, 0, false), 0, false).is_err() {
            /* Never: the own slot's table is the driver's -- but a caller
             * told, not left waiting */
            q.slots[t] = Slot::EMPTY;
            q.exclusive = false;
            q.flush_call = Some((done, ctx));
            return;
        }
        q.owed |= 1 << t;
        self.ring(q);
    }

    /// What the device has finished, ended: from the HBA's interrupt, from
    /// a waiter polling the port, from the error-handling task's look.
    /// Nothing while the error handler has the port. Whether an error or a
    /// change of the link was seen -- the error handler wanted.
    pub(crate) fn reap(&self) -> bool {
        let mut calls = Calls::new();
        let mut raised = false;
        let mut short = 0u32;
        {
            let mut q = self.queue.lock();
            if q.recovering || q.failed {
                return false;
            }
            let is = self.port.read(PX_IS);
            if is != 0 {
                self.port.write(PX_IS, is);
            }
            if is & (IS_ERRORS | IS_LINK) != 0 {
                q.seen_is |= is;
                q.seen_tfd = self.port.read(PX_TFD);
                self.attention.store(true, Ordering::Release);
                raised = true;
            }
            if q.issued != 0 {
                let still = self.port.read(PX_CI) | if self.ncq { self.port.read(PX_SACT) } else { 0 };
                let done = q.issued & !still;
                if done != 0 {
                    /* What the HBA wrote -- the data, the byte count -- read
                     * after it said it was done */
                    kcore::barrier::dma_rmb();
                    q.issued &= !done;
                    q.unfinished = 0;
                    for t in bits(done) {
                        let ok = q.moved_all(t);
                        if !ok {
                            short |= 1 << t;
                        }
                        self.finish(&mut q, t, ok, &mut calls);
                    }
                }
            }
            self.start_flush(&mut q);
        }
        calls.run(&self.ended);
        if short != 0 {
            klog!(0, "ahci: {}: {} commands moved fewer bytes than asked, and failed (slots {:#x})", self.name(),
                short.count_ones(), short);
        }
        raised
    }

    /// One turn of a wait for slot `slot` (or for the port): with the
    /// interrupt to wait on, the CPU given to whoever else can run -- and,
    /// for a wait begun at `nap_since` past `spin_ns` from then, a nap of
    /// `nap_ns` instead -- the error handler run here first if it is wanted
    /// and nobody runs it; polling, the port looked at and the handler run
    /// here. A wait for a slot's end sleeps on its event past the spin
    /// instead (`wait_slot`).
    fn idle_on(&self, slot: Option<usize>, poll: bool, nap_since: Option<u64>) {
        if !poll && self.interrupt_mode() {
            let now = boot_time_ns();
            if self.attention.load(Ordering::Acquire) || slot.is_some_and(|t| self.overdue(t, now)) {
                self.service();
            }
            match nap_since {
                Some(since) if now.saturating_sub(since) >= self.spin_ns => {
                    kcore::task::sleep(Duration::from_nanos(self.nap_ns));
                }
                _ => kcore::task::yield_to_runnable(),
            }
        } else {
            self.reap();
            self.service();
            relax();
        }
    }

    fn overdue(&self, t: usize, now: u64) -> bool {
        let d = self.deadline[t].load(Ordering::Relaxed);
        d != 0 && now > d
    }

    fn overdue_mask(&self, now: u64) -> u32 {
        (0..MAX_SLOTS).filter(|&t| self.overdue(t, now)).fold(0u32, |m, t| m | 1 << t)
    }

    /// A queued slot for a task, waited for while there is none free -- or
    /// the port is the error handler's or one of the disk's own commands'.
    /// None for a disk given up on. Waited for awake, giving the CPU to
    /// whoever else can run, and never asleep: a slot freed goes to whoever
    /// looks first, and the task that gave it back takes it again at once
    /// for its next command -- one asleep lost every slot to them: with 32
    /// tasks on the SMR disk's 31, the 32nd waited six seconds for one,
    /// where it had waited under two awake. So only more tasks than the
    /// disk queues cost a CPU here.
    fn take_slot(&self) -> Option<usize> {
        loop {
            if let Some(t) = self.try_slot() {
                return Some(t);
            }
            if self.queue.lock().failed {
                return None;
            }
            self.idle_on(None, false, None);
        }
    }

    /// The same, but None at once when there is none to take now.
    fn try_slot(&self) -> Option<usize> {
        let mut q = self.queue.lock();
        if q.failed || q.exclusive || q.recovering || q.free == 0 {
            return None;
        }
        let t = q.free.trailing_zeros() as usize;
        q.free &= !(1 << t);
        q.slots[t] = Slot { waiter: Some(Waiter::Task), ..Slot::EMPTY };
        self.outcome[t].store(PENDING, Ordering::Relaxed);
        Some(t)
    }

    /// A slot a task held given back, its command ended.
    fn give_back(&self, t: usize) {
        let mut q = self.queue.lock();
        q.slots[t] = Slot::EMPTY;
        if self.queued_mask() & (1 << t) != 0 {
            q.free |= 1 << t;
        }
    }

    /// Slot `t`'s command waited for: whether it went through. The slot is
    /// still the task's. Looked for, the CPU given to whoever else can run,
    /// for `spin_ns`; then asleep on the slot's event, which the command's
    /// end signals once `finish` has set its outcome and the lock is let go
    /// of -- a signal that comes between the look and the sleep ends the
    /// sleep at once -- waking at the error handler's tick to see that the
    /// command is not out past its time. A signal left from the slot's last
    /// command costs a turn.
    fn wait_slot(&self, t: usize, poll: bool) -> bool {
        let since = boot_time_ns();
        loop {
            match self.outcome[t].load(Ordering::Acquire) {
                DONE_OK => return true,
                DONE_FAILED => return false,
                _ => {}
            }
            let now = boot_time_ns();
            if poll || !self.interrupt_mode() || now.saturating_sub(since) < self.spin_ns {
                self.idle_on(Some(t), poll, Some(since));
                continue;
            }
            if self.attention.load(Ordering::Acquire) || self.overdue(t, now) {
                self.service();
            }
            let _ = self.ended[t].wait_for(Duration::from_millis(EH_TICK_MS));
        }
    }

    /* ---- what the block layer asks ---- */

    /// Whole sectors to or from `addr`, `len` bytes, from `lba` on: in
    /// commands of at most `per_command`, the buffer the device's. With
    /// `fua`, a write each command of which the disk has made durable when
    /// it ends -- asked only of a disk with FUA.
    pub(crate) fn transfer(&self, lba: u64, addr: usize, len: usize, write: bool, fua: bool) -> bool {
        let size = self.id.sector_size as usize;
        if len == 0 || len % size != 0 || !self.within(lba, (len / size) as u64) {
            klog!(0, "ahci: {}: a {} of {} bytes at {} is none this disk takes", self.name(),
                if write { "write" } else { "read" }, len, lba);
            return false;
        }
        let per_command = self.per_command();
        let mut done = 0usize;
        while done < len {
            let bytes = per_command.min(len - done);
            let count = (bytes / size) as u64;
            let at = lba + (done / size) as u64;
            let Some(t) = self.take_slot() else { return false };
            let built = {
                let mut q = self.queue.lock();
                let s64a = self.s64a();
                let built = q.prdt_virt(t, addr + done, bytes, s64a).and_then(|prds| {
                    let fis = self.data_fis(write, write && fua && self.id.fua, at, count, t);
                    q.command(t, &fis, prds, write)
                });
                if built.is_ok() {
                    q.slots[t] = self.data_slot(Waiter::Task, write, at, count, bytes);
                    q.owed |= 1 << t;
                    self.ring(&mut q);
                }
                built
            };
            let ok = match built {
                Ok(()) => self.wait_slot(t, false),
                Err(fault) => {
                    self.say(write, at, count, fault);
                    false
                }
            };
            self.give_back(t);
            if !ok {
                return false;
            }
            done += bytes;
        }
        true
    }

    /// A batch (`read_pieces`, `write_pieces`): its pieces, each `len`
    /// bytes at `phys + at` of the batch's buffer, a command for every run
    /// of them that follow one another on the disk -- up to the queue's
    /// depth of them out at once, the doorbell rung once for each lot,
    /// waited for in the order they went. A failure ends the issuing, not
    /// the waiting: nothing returns while a command of the batch may still
    /// move data into or out of the buffer.
    pub(crate) fn batch(&self, write: bool, phys: u64, base: u64, pieces: &[Piece]) -> bool {
        let size = self.id.sector_size as usize;
        let per_command = self.per_command();
        let mut window = [0usize; MAX_SLOTS];
        let (mut head, mut out, mut next) = (0usize, 0usize, 0usize);
        let mut ok = true;
        loop {
            while ok && next < pieces.len() && out < self.depth as usize {
                let slot = if out == 0 { self.take_slot() } else { self.try_slot() };
                let Some(t) = slot else {
                    ok &= out != 0;
                    break;
                };
                let Some(lba) = base.checked_add(pieces[next].sector) else {
                    self.give_back(t);
                    ok = false;
                    break;
                };
                let built = {
                    let mut q = self.queue.lock();
                    let s64a = self.s64a();
                    let built = q.prdt_pieces(t, phys, &pieces[next..], size, per_command, s64a)
                        .and_then(|(taken, bytes, prds)| {
                            let count = (bytes / size) as u64;
                            if !self.within(lba, count) {
                                return Err(Fault::Buffer);
                            }
                            let fis = self.data_fis(write, false, lba, count, t);
                            q.command(t, &fis, prds, write).map(|()| (taken, bytes, count))
                        });
                    if let Ok((_, bytes, count)) = built {
                        q.slots[t] = self.data_slot(Waiter::Task, write, lba, count, bytes);
                        q.owed |= 1 << t;
                    }
                    built
                };
                match built {
                    Ok((taken, _, _)) => {
                        window[(head + out) % MAX_SLOTS] = t;
                        out += 1;
                        next += taken;
                    }
                    Err(fault) => {
                        self.say(write, lba, (pieces[next].len / size) as u64, fault);
                        self.give_back(t);
                        ok = false;
                    }
                }
            }
            {
                let mut q = self.queue.lock();
                self.ring(&mut q);
            }
            if out == 0 {
                return ok;
            }
            let t = window[head];
            head = (head + 1) % MAX_SLOTS;
            out -= 1;
            ok &= self.wait_slot(t, false);
            self.give_back(t);
        }
    }

    /// One asynchronous request: into a queued slot of its own, its
    /// doorbell left to `kick` unless `kick`. Never blocks and never waits: a
    /// full queue, or a port the error handler or one of the disk's own
    /// commands has, is Busy for the caller to try again. A flush is the
    /// synchronous path's -- it drains the queue -- and Unsupported here,
    /// which the block layer's queue does by that path.
    pub(crate) fn submit_io(&self, io: &BlockIo, kick: bool) -> Result<(), SubmitError> {
        let write = match io.op {
            IO_READ => false,
            IO_WRITE => true,
            IO_FLUSH => return self.submit_flush(io, kick),
            _ => return Err(SubmitError::Invalid),
        };
        let fua = write && io.fua != 0;
        if fua && !self.id.fua {
            return Err(SubmitError::Unsupported);
        }
        let size = self.id.sector_size as usize;
        let count = u64::from(io.count);
        let bytes = io.count as usize * size;
        let count_max = if self.id.lba48 { LBA48_COUNT_MAX } else { LBA28_COUNT_MAX };
        if !self.within(io.sector, count) || bytes > ASYNC_MAX || count > count_max {
            return Err(SubmitError::Invalid);
        }
        let mut q = self.queue.lock();
        if q.failed {
            return Err(SubmitError::Invalid);
        }
        if q.exclusive || q.recovering || q.free == 0 {
            return Err(SubmitError::Busy);
        }
        let t = q.free.trailing_zeros() as usize;
        let s64a = self.s64a();
        let built = q.prdt_contiguous(t, io.phys, bytes, s64a).and_then(|prds| {
            let fis = self.data_fis(write, fua, io.sector, count, t);
            q.command(t, &fis, prds, write)
        });
        if built.is_err() {
            return Err(SubmitError::Invalid);
        }
        q.free &= !(1 << t);
        q.slots[t] = self.data_slot(Waiter::Call { done: io.done, ctx: io.ctx as usize }, write, io.sector, count, bytes);
        q.owed |= 1 << t;
        if kick {
            self.ring(&mut q);
        }
        Ok(())
    }

    /// An asynchronous flush: the queue drained -- nothing queued issued
    /// meanwhile, Busy to whoever asks -- and FLUSH CACHE then, on the disk's
    /// own slot, its end the caller's. A disk with no write cache has nothing
    /// to push: Unsupported, which the block layer's queue does as a flush of
    /// nothing. So is one whose own slot is a queued one's.
    fn submit_flush(&self, io: &BlockIo, kick: bool) -> Result<(), SubmitError> {
        if self.id.flush == Flush::None || self.own < self.depth as usize {
            return Err(SubmitError::Unsupported);
        }
        let mut q = self.queue.lock();
        if q.failed {
            return Err(SubmitError::Invalid);
        }
        if q.exclusive || q.recovering {
            return Err(SubmitError::Busy);
        }
        q.exclusive = true;
        q.flush_call = Some((io.done, io.ctx as usize));
        self.start_flush(&mut q);
        let _ = kick;
        Ok(())
    }

    /// The doorbell for what `submit_io` queued without one.
    pub(crate) fn ring_owed(&self) {
        let mut q = self.queue.lock();
        self.ring(&mut q);
    }

    /// One of the disk's own commands -- not queued: a flush, a standby --
    /// with the port to itself: the queue drained first, and nothing queued
    /// issued meanwhile. `poll` waits by looking at the port whatever the
    /// interrupt does. Whether it went through.
    pub(crate) fn own_command(&self, what: &'static str, command: u8, ms: u64, poll: bool) -> bool {
        let since = boot_time_ns();
        loop {
            {
                let mut q = self.queue.lock();
                if q.failed {
                    return false;
                }
                if !q.exclusive && !q.recovering {
                    q.exclusive = true;
                    break;
                }
            }
            self.idle_on(None, poll, Some(since));
        }
        loop {
            {
                let mut q = self.queue.lock();
                if q.failed {
                    q.exclusive = false;
                    return false;
                }
                if q.issued == 0 && q.owed == 0 && q.free == self.queued_mask() {
                    break;
                }
            }
            self.idle_on(None, poll, Some(since));
        }
        let t = self.own;
        let built = {
            let mut q = self.queue.lock();
            q.slots[t] = Slot { waiter: Some(Waiter::Task), what, queued: false, timeout_ms: ms, tries: MAX_TRIES,
                                ..Slot::EMPTY };
            self.outcome[t].store(PENDING, Ordering::Relaxed);
            let built = q.command(t, &fis_dma(command, 0, 0, false), 0, false);
            if built.is_ok() {
                q.owed |= 1 << t;
                self.ring(&mut q);
            }
            built
        };
        let ok = built.is_ok() && self.wait_slot(t, poll);
        let mut q = self.queue.lock();
        q.slots[t] = Slot::EMPTY;
        q.exclusive = false;
        ok
    }

    /// The write cache pushed out, where the disk has a command for it.
    pub(crate) fn flush_cache(&self) -> bool {
        match self.id.flush {
            Flush::None => true,
            Flush::Flush => self.own_command("flush", cmd::FLUSH_CACHE, FLUSH_MS, false),
            Flush::FlushExt => self.own_command("flush", cmd::FLUSH_CACHE_EXT, FLUSH_MS, false),
        }
    }

    /// The disk told the power is about to go: STANDBY IMMEDIATE.
    pub(crate) fn standby(&self) -> bool {
        self.own_command("standby", cmd::STANDBY_IMMEDIATE, STANDBY_MS, false)
    }

    /// A command that changes nothing, waited for by looking at the port:
    /// what `Hba::check_interrupt` watches the interrupt of.
    pub(crate) fn check_power_mode(&self) -> bool {
        self.own_command("check power mode", cmd::CHECK_POWER_MODE, IDENTIFY_MS, true)
    }

    /// A command that failed before it reached the disk, or ended failed:
    /// said.
    fn say(&self, write: bool, lba: u64, count: u64, fault: Fault) {
        let (bus, slot, func, index) = self.at;
        klog!(0, "ahci: {} ({:02x}:{:02x}.{} port {}): {} of {} sectors at {}: {}, serr {:#x} ssts {:#x}",
            self.name(), bus, slot, func, index, if write { "write" } else { "read" }, count, lba, fault.describe(),
            self.port.read(PX_SERR), self.port.read(PX_SSTS));
    }

    /* ---- the error handler ---- */

    /// The error handler, if it is wanted: an error or a link change seen,
    /// or a command out past its time. Whoever gets here first does it, the
    /// rest go on waiting.
    pub(crate) fn service(&self) {
        let now = boot_time_ns();
        if !self.attention.load(Ordering::Acquire) && self.overdue_mask(now) == 0 {
            return;
        }
        let Some(_owner) = self.eh.try_lock() else { return };
        self.recover(now);
    }

    /// The port taken back, and every command it had accounted for: those
    /// that finished before it all went wrong ended well; the one the disk
    /// names as failed -- or the only one out, or each found alone -- failed;
    /// the rest, lost with it, sent again (MAX_TRIES). A port that will not
    /// come back, or MAX_UNFINISHED commands running that did not finish, and
    /// the disk is given up on.
    fn recover(&self, now: u64) {
        let mut calls = Calls::new();
        let (out, is, tfd, overdue) = {
            let mut q = self.queue.lock();
            if q.failed || q.recovering {
                return;
            }
            let overdue = self.overdue_mask(now) & q.issued;
            let is = core::mem::take(&mut q.seen_is) | self.port.read(PX_IS);
            let tfd = match core::mem::take(&mut q.seen_tfd) {
                0 => self.port.read(PX_TFD),
                seen => seen,
            };
            let attention = self.attention.swap(false, Ordering::AcqRel);
            if !attention && overdue == 0 && is & (IS_ERRORS | IS_LINK) == 0 {
                return;
            }
            /* Nothing more issued or reaped by anyone else, and no interrupt
             * from the port while it is the handler's */
            q.recovering = true;
            self.port.write(PX_IE, 0);
            (q.issued, is, tfd, overdue)
        };

        let (sact, ci) = (self.port.read(PX_SACT), self.port.read(PX_CI));
        let finished = out & !(sact | ci);
        let mut left = out & !finished;
        let device = is & IS_TFES != 0;
        let failed = is & IS_ERRORS != 0;
        let link = is & IS_LINK != 0;
        let cause = if device {
            "the disk failed a command"
        } else if failed {
            "the HBA failed a command"
        } else if link {
            "the link changed under the commands"
        } else if overdue != 0 {
            "a command is out past its time"
        } else {
            "the port wants looking at"
        };
        let (bus, slot, func, index) = self.at;
        klog!(0, "ahci: {} ({:02x}:{:02x}.{} port {}): {}: {} out (sact {:#x} ci {:#x}), is {:#x} {}, serr {:#x} ssts {:#x}",
            self.name(), bus, slot, func, index, cause, out.count_ones(), sact, ci, is, status_error(tfd),
            self.port.read(PX_SERR), self.port.read(PX_SSTS));

        let unfinished = {
            let mut q = self.queue.lock();
            q.issued = 0;
            for t in bits(finished) {
                let ok = q.moved_all(t);
                self.finish(&mut q, t, ok, &mut calls);
            }
            if finished != 0 {
                q.unfinished = 0;
            }
            if !failed && (link || overdue != 0) {
                q.unfinished += 1;
            }
            q.unfinished
        };
        calls.run(&self.ended);

        /* A timeout or a lost link leaves the device in no known state: a
         * COMRESET. An error it said leaves it waiting to be asked again. */
        if !self.port.recover(!failed) {
            self.give_up(left, "the port did not come back");
            return;
        }
        if unfinished >= MAX_UNFINISHED {
            self.give_up(left, "commands in a row did not finish");
            return;
        }

        if device && left != 0 {
            let queued = {
                let q = self.queue.lock();
                bits(left).any(|t| q.slots[t].queued)
            };
            /* After a queued command fails the disk takes no more queued
             * ones until its error log is read -- which also names the one */
            let named = if queued { self.read_error_log() } else { None };
            match named {
                Some(e) if !e.non_queued && left & (1 << e.tag) != 0 => {
                    let t = usize::from(e.tag);
                    self.fail(t, &alloc::format!("failed, status {:#x} error {:#x} (the disk's error log)", e.status,
                        e.error));
                    left &= !(1 << t);
                }
                _ if left.count_ones() == 1 => {
                    let t = left.trailing_zeros() as usize;
                    self.fail(t, &alloc::format!("failed, {}", status_error(tfd)));
                    left = 0;
                }
                _ => {
                    match self.one_at_a_time(left) {
                        Some(()) => left = 0,
                        None => return,
                    }
                }
            }
        }

        let mut q = self.queue.lock();
        for t in bits(left) {
            if q.slots[t].tries >= MAX_TRIES {
                self.finish(&mut q, t, false, &mut calls);
                continue;
            }
            q.slots[t].tries += 1;
            q.reset_moved(t);
            q.owed |= 1 << t;
        }
        if left != 0 {
            klog!(1, "ahci: {}: {} commands sent again", self.name(), left.count_ones());
        }
        q.recovering = false;
        self.port.write(PX_IS, u32::MAX);
        if self.hba.interrupts_work() {
            self.port.write(PX_IE, IE_ALL);
        }
        self.ring(&mut q);
        self.start_flush(&mut q);
        drop(q);
        calls.run(&self.ended);
    }

    /// Slot `t`'s command ended failed, and said: `why`.
    fn fail(&self, t: usize, why: &str) {
        let mut calls = Calls::new();
        let (what, lba, count) = {
            let mut q = self.queue.lock();
            let s = q.slots[t];
            self.finish(&mut q, t, false, &mut calls);
            (s.what, s.lba, s.count)
        };
        let (bus, slot, func, index) = self.at;
        klog!(0, "ahci: {} ({:02x}:{:02x}.{} port {}): {} of {} sectors at {}: {}", self.name(), bus, slot, func,
            index, what, count, lba, why);
        calls.run(&self.ended);
    }

    /// Slot `t`'s command, built already, sent alone and waited for by
    /// looking at the port, which is the error handler's.
    fn run_alone(&self, t: usize) -> Result<(), Fault> {
        let (queued, ms) = {
            let mut q = self.queue.lock();
            q.reset_moved(t);
            (q.slots[t].queued, q.slots[t].timeout_ms)
        };
        self.port.write(PX_IS, u32::MAX);
        kcore::barrier::dma_wmb();
        if queued {
            self.port.write(PX_SACT, 1 << t);
        }
        self.port.write(PX_CI, 1 << t);
        let mut deadline = Deadline::after_ms(ms);
        loop {
            let is = self.port.read(PX_IS);
            if is & IS_ERRORS != 0 {
                return Err(Fault::Device { is, tfd: self.port.read(PX_TFD) });
            }
            if (self.port.read(PX_CI) | self.port.read(PX_SACT)) & (1 << t) == 0 {
                kcore::barrier::dma_rmb();
                return Ok(());
            }
            if is & IS_LINK != 0 {
                return Err(Fault::Link { is, ssts: self.port.read(PX_SSTS) });
            }
            if deadline.passed() {
                return Err(Fault::Timeout { is, tfd: self.port.read(PX_TFD), ci: self.port.read(PX_CI) });
            }
            relax();
        }
    }

    /// The NCQ Command Error log, read on the disk's own slot: what it names,
    /// or None where the disk keeps none, it does not read, or it names
    /// nothing believable. Reading it is what takes the disk out of a failed
    /// queued command's state; where it cannot be read, a COMRESET does.
    fn read_error_log(&self) -> Option<NcqError> {
        let t = self.own;
        let built = if self.id.gpl {
            let mut q = self.queue.lock();
            let scratch = q.scratch();
            let s64a = self.s64a();
            let built = q.put_prd(t, 0, scratch, LOG_PAGE_BYTES, s64a)
                .and_then(|()| q.command(t, &fis_read_log(LOG_NCQ_ERROR), 1, false));
            q.slots[t] = Slot { what: "error log", bytes: LOG_PAGE_BYTES as u32, timeout_ms: LOG_MS, tries: MAX_TRIES,
                                ..Slot::EMPTY };
            built.is_ok()
        } else {
            false
        };
        let read = built && self.run_alone(t).is_ok();
        let named = {
            let mut q = self.queue.lock();
            let named = if read && q.moved(t) == LOG_PAGE_BYTES as u32 {
                q.scratch_bytes().and_then(ncq_error)
            } else {
                None
            };
            q.slots[t] = Slot::EMPTY;
            named
        };
        if named.is_none() {
            klog!(0, "ahci: {}: the disk's NCQ error log {}: the port reset instead", self.name(),
                if !self.id.gpl { "is not kept" } else if read { "names nothing to believe" } else { "did not read" });
            if !self.port.recover(true) {
                return None;
            }
        }
        named
    }

    /// The commands of `left`, built already, each sent alone so that the
    /// one that fails fails alone: what the disk did not name when they were
    /// out together. None when the port did not come back between them, the
    /// disk given up on; every command of `left` ended either way.
    fn one_at_a_time(&self, left: u32) -> Option<()> {
        klog!(0, "ahci: {}: the failed command not named: the {} out each sent again alone", self.name(),
            left.count_ones());
        let mut rest = left;
        for t in bits(left) {
            rest &= !(1 << t);
            match self.run_alone(t) {
                Ok(()) => {
                    let mut calls = Calls::new();
                    {
                        let mut q = self.queue.lock();
                        let ok = q.moved_all(t);
                        self.finish(&mut q, t, ok, &mut calls);
                    }
                    calls.run(&self.ended);
                }
                Err(fault) => {
                    self.fail(t, &fault.describe());
                    let queued = matches!(fault, Fault::Device { .. });
                    if !self.port.recover(!queued) {
                        self.give_up(rest, "the port did not come back");
                        return None;
                    }
                    if queued && self.ncq {
                        let _ = self.read_error_log();
                    }
                }
            }
        }
        Some(())
    }

    /// The disk failed for good: every command of `left` and every one owed
    /// ended failed, the port stopped -- so that the HBA does nothing more
    /// in its memory -- and every request of it refused from here on at
    /// once: said once, here, rather than as a timeout a request.
    fn give_up(&self, left: u32, why: &str) {
        let mut calls = Calls::new();
        {
            let mut q = self.queue.lock();
            q.failed = true;
            let owed = q.owed;
            q.owed = 0;
            q.issued = 0;
            for t in bits(left | owed) {
                self.finish(&mut q, t, false, &mut calls);
            }
            if let Some((done, ctx)) = q.flush_call.take() {
                calls.push(done, ctx, STATUS_FAILED);
            }
            q.exclusive = false;
        }
        self.port.write(PX_IE, 0);
        let _ = self.port.stop();
        let (bus, slot, func, index) = self.at;
        klog!(0, "ahci: {} ({:02x}:{:02x}.{} port {}): {} (tfd {:#x} cmd {:#x} ssts {:#x}): the disk is failed, \
                  and every request of it refused from here on",
            self.name(), bus, slot, func, index, why, self.port.read(PX_TFD), self.port.read(PX_CMD),
            self.port.read(PX_SSTS));
        calls.run(&self.ended);
    }
}

/// IDENTIFY DEVICE at bring-up, on slot 0 of a queue nobody else has yet,
/// polled: its words.
pub(crate) fn identify(port: &PortRegs, queue: &mut Queue) -> Result<[u16; ataproto::identify::WORDS], Fault> {
    let s64a = port.cap & CAP_S64A != 0;
    let scratch = queue.scratch();
    queue.put_prd(0, 0, scratch, SCRATCH_BYTES, s64a)?;
    queue.command(0, &fis_dma(cmd::IDENTIFY, 0, 0, false), 1, false)?;
    port.write(PX_IS, u32::MAX);
    kcore::barrier::dma_wmb();
    port.write(PX_CI, 1);
    let mut deadline = Deadline::after_ms(IDENTIFY_MS);
    loop {
        let is = port.read(PX_IS);
        if is & IS_ERRORS != 0 {
            return Err(Fault::Device { is, tfd: port.read(PX_TFD) });
        }
        if port.read(PX_CI) & 1 == 0 {
            break;
        }
        if deadline.passed() {
            return Err(Fault::Timeout { is, tfd: port.read(PX_TFD), ci: port.read(PX_CI) });
        }
        relax();
    }
    kcore::barrier::dma_rmb();
    let tfd = port.read(PX_TFD);
    if tfd & (TFD_ERR | TFD_DF) != 0 {
        return Err(Fault::Device { is: port.read(PX_IS), tfd });
    }
    let moved = queue.moved(0);
    if moved as usize != SCRATCH_BYTES {
        return Err(Fault::Short { moved, asked: SCRATCH_BYTES });
    }
    let bytes = queue.scratch_bytes().ok_or(Fault::Buffer)?;
    let mut words = [0u16; ataproto::identify::WORDS];
    for (word, pair) in words.iter_mut().zip(bytes.chunks_exact(2)) {
        *word = u16::from_le_bytes([pair[0], pair[1]]);
    }
    Ok(words)
}
