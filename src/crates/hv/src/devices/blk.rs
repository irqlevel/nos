//! A virtio block device, legacy PCI: a guest's `/dev/vda`, over whatever
//! stores its bytes (`Backend`) -- a file of nos's, in the module, read and
//! written by a task of its own.
//!
//! A request is a chain of buffers: a 16-byte header the driver wrote (the
//! type and, for a read or a write, the sector), the data, and a status byte
//! the device writes last. It is handled whatever the layout -- the header,
//! the data and the status taken as streams across the chain's readable and
//! writable buffers.
//!
//! The disk works beside the guest, as a disk of its own would. When the
//! driver notifies, the vCPU takes what it made available off the ring --
//! the data of a write copied out of guest memory then, into a buffer of the
//! device's -- and hands each request to the backend, which serves them
//! while the guest runs on: in whatever order it likes, several at once if
//! it can -- virtio orders nothing between requests in flight; a driver
//! that needs a write done before another waits for it, and before a
//! flush. Each time round the run loop (`poll`) what the backend has served
//! is given back: a read's data copied into the guest, the status written,
//! the chain on the used ring, and an interrupt.
//! The guest's memory is only ever touched here, on the vCPU's task: the
//! backend sees buffers of the device's and nothing else.
//!
//! A discard and a write of zeros come as ranges -- sectors, a count, and
//! for a write of zeros whether the blocks may be given back -- which the
//! device copies out of the guest's memory once, checks there, and hands
//! the backend that copy (`Request::ranges`): a guest that changes them
//! after the check changes nothing it was checked on.
//!
//! At most `IN_FLIGHT` requests are with the backend at once, each in a
//! buffer of `REQUEST_BYTES` taken when the device is made -- what the driver
//! is told a request may carry, `SEG_MAX` segments of at most `SIZE_MAX` -- so
//! nothing is allocated on the way, and a guest that keeps its ring full
//! waits for its disk rather than growing anything of the host's: what does
//! not fit stays on the ring until a buffer comes back.

use alloc::boxed::Box;
use alloc::vec::Vec;

use super::pci::Identity;
use super::virtio::{self, Asked, Broken, Queue, Raise, Seg, Transport};
use crate::memory::GuestMemory;
use crate::{Error, Result};

/// What stores a disk's bytes, and serves the requests the device hands it
/// -- beside the guest, from tasks of its own or however it likes, several
/// at once and in any order -- until `take` gives each back.
pub trait Backend: Send {
    /// The disk's size in bytes, a whole number of sectors: what the device
    /// keeps every request inside.
    fn size(&self) -> u64;
    /// A disk the guest may only read: the device says so to the driver,
    /// and refuses a write itself rather than hand it here.
    fn read_only(&self) -> bool {
        false
    }
    /// Serve `req`. The device never has more than `IN_FLIGHT` handed over
    /// and not yet taken back, and a backend takes that many.
    fn submit(&mut self, req: Request);
    /// A request served, if one is: in the order they were served, which
    /// need not be the order they were submitted in.
    fn take(&mut self) -> Option<Request>;
}

/// What a request asks of the disk.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Read,
    Write,
    /// Everything written so far, on stable storage.
    Flush,
    /// Its ranges (`Request::ranges`) no longer needed by the guest: what a
    /// backend may give back, and need not.
    Discard,
    /// Its ranges made zeros -- given back too, where a range says it may be.
    WriteZeroes,
}

impl Op {
    /// Whether it changes what the disk holds.
    pub fn writes(self) -> bool {
        matches!(self, Op::Write | Op::Discard | Op::WriteZeroes)
    }
}

/// One range of a discard or a write of zeros: `len` bytes from `offset`,
/// whole sectors inside the disk, and whether -- for a write of zeros --
/// the blocks under it may be given back rather than written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Range {
    pub offset: u64,
    pub len: u64,
    pub unmap: bool,
}

/// One request, on its way to the backend and back. Its buffer is the
/// device's, and so is the request: a backend serves one it was handed and
/// gives it back, and has no way to make one of its own.
pub struct Request {
    op: Op,
    offset: u64,
    len: usize,
    buf: Box<[u8]>,
    ok: bool,
    slot: usize,
}

impl Request {
    pub fn op(&self) -> Op {
        self.op
    }

    /// Where on the disk, in bytes: whole sectors, inside it (a read or a
    /// write).
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// A write's data.
    pub fn data(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// Where a read's data goes: exactly as many bytes as the guest asked
    /// for.
    pub fn data_mut(&mut self) -> &mut [u8] {
        &mut self.buf[..self.len]
    }

    /// How it went: false for an error the guest is to see.
    pub fn done(&mut self, ok: bool) {
        self.ok = ok;
    }

    /// A discard's or a write of zeros' ranges, as the device checked them
    /// in its own copy: every one whole sectors inside the disk. None for
    /// any other request.
    pub fn ranges(&self) -> impl Iterator<Item = Range> + '_ {
        let ranges = if matches!(self.op, Op::Discard | Op::WriteZeroes) { &self.buf[..self.len] } else { &[] };
        ranges.chunks_exact(RANGE_BYTES).map(|r| {
            let sector = u64::from_le_bytes([r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7]]);
            let count = u32::from_le_bytes([r[8], r[9], r[10], r[11]]);
            let flags = u32::from_le_bytes([r[12], r[13], r[14], r[15]]);
            /* Checked to fit when the device took them (`ranges_request`) */
            Range {
                offset: sector.saturating_mul(SECTOR),
                len: u64::from(count) * SECTOR,
                unmap: flags & RANGE_UNMAP != 0,
            }
        })
    }
}

/// Virtio's type for a block device.
const VIRTIO_TYPE: u16 = 2;
/// Mass storage, SCSI -- what a legacy virtio disk has always said it is.
const CLASS: u32 = 0x01_00_00;
/// Its I/O BAR: the header -- MSI-X's longer one -- and the configuration
/// below, rounded up to a power of two. A configuration past the BAR's end
/// is no device's to a guest: it reads all ones there, and a discard
/// alignment of all ones once made every trim of a guest's trim nothing.
pub const BAR_SIZE: u32 = 0x80;
const _: () = assert!(virtio::DEVICE_CONFIG_MSIX as usize + CFG_BYTES <= BAR_SIZE as usize,
                      "the configuration fits the BAR, MSI-X's header and all");

pub const SECTOR: u64 = 512;
/// The queue's size. A request takes a descriptor a segment plus two, so a
/// ring this size holds three of the largest at once -- and the device has
/// one on its way back while the backend serves the next.
const QUEUE_SIZE: u16 = 256;
/// The most chains taken off the queue in one pass: a queue's worth, all a
/// driver can have available at once. A request answered here -- a GET_ID,
/// one past the disk -- takes no buffer, so running out of buffers does not
/// end a pass of them; unbounded, a guest that kept the ring topped up with
/// such requests from a second CPU held the CPU running the pass in the host
/// for good (hv-fuzz's racing CPU). What is left is the next round's.
const PASS: usize = QUEUE_SIZE as usize;

/// Features: a limit on a segment's size and on the segments a request may
/// have, flush, a disk that is read-only, and -- one that is not -- discard
/// and a write of zeros.
const F_SIZE_MAX: u32 = 1 << 1;
const F_SEG_MAX: u32 = 1 << 2;
const F_RO: u32 = 1 << 5;
const F_FLUSH: u32 = 1 << 9;
const F_DISCARD: u32 = 1 << 13;
const F_WRITE_ZEROES: u32 = 1 << 14;
/// A discard's or a write of zeros' ranges at most, and a range's sectors:
/// what the driver is told. A range is a sector, a count and flags.
const MAX_RANGES: usize = 8;
const MAX_RANGE_SECTORS: u32 = 1 << 17;
const RANGE_BYTES: usize = 16;
/// A write of zeros' range may have its blocks given back.
const RANGE_UNMAP: u32 = 1;
/// The sectors a discard is best aligned to: 4 KiB, the block an image on
/// nos's filesystem gives back whole.
const DISCARD_ALIGNMENT: u32 = 8;
/// What a request may carry, which the driver is told: data segments of at
/// most a page, and this many of them.
const SIZE_MAX: u32 = 4096;
const SEG_MAX: u32 = 64;
/// The most data one request has, and so the size of a buffer.
pub const REQUEST_BYTES: usize = (SIZE_MAX * SEG_MAX) as usize;
/// The most requests with the backend at once, and so how many buffers the
/// device has.
pub const IN_FLIGHT: usize = 8;

/// Request types, and statuses.
const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;
const T_GET_ID: u32 = 8;
const T_DISCARD: u32 = 11;
const T_WRITE_ZEROES: u32 = 13;
const S_OK: u8 = 0;
const S_IOERR: u8 = 1;
const S_UNSUPP: u8 = 2;
const HEADER: usize = 16;
/// What GET_ID answers with, at most: the id's length in the spec.
const ID_BYTES: usize = 20;

/// The configuration, by offset past the header.
const CFG_CAPACITY: u16 = 0;
const CFG_SIZE_MAX: u16 = 8;
const CFG_SEG_MAX: u16 = 12;
const CFG_MAX_DISCARD_SECTORS: u16 = 36;
const CFG_MAX_DISCARD_SEG: u16 = 40;
const CFG_DISCARD_ALIGNMENT: u16 = 44;
const CFG_MAX_ZEROES_SECTORS: u16 = 48;
const CFG_MAX_ZEROES_SEG: u16 = 52;
const CFG_ZEROES_MAY_UNMAP: u16 = 56;
const CFG_BYTES: usize = 60;

/// What the device has done, for a report.
#[derive(Clone, Copy, Default)]
pub struct Stats {
    pub reads: u64,
    pub writes: u64,
    pub flushes: u64,
    pub read_bytes: u64,
    pub written_bytes: u64,
    pub errors: u64,
    pub discards: u64,
    pub discarded_bytes: u64,
    pub zeroings: u64,
    pub zeroed_bytes: u64,
}

/// A request with the backend, as the device keeps it: the chain it came in
/// on -- its head, and its buffers, where a read's data goes and the status
/// after it. What it asked comes back with the request.
struct Slot {
    /// The buffer, while the device has it; with the backend otherwise.
    buf: Option<Box<[u8]>>,
    /// The chain being served, until it is given back. None when the slot is
    /// free -- or when a reset forgot its chain while the backend still has
    /// the buffer, which is then taken back and given to nobody.
    head: Option<u16>,
    /// The chain's buffers: room for the longest chain the queue holds.
    segs: Vec<Seg>,
}

/// What a request comes to once its header is read.
enum Next {
    /// The backend's to serve: what, where, and how many bytes.
    Serve(Op, u64, usize),
    /// A discard's or a write of zeros' ranges, the backend's to serve: how
    /// many, in the device's own copy of them, checked.
    Ranges(Op, usize, [u8; MAX_RANGES * RANGE_BYTES]),
    /// Answered here: the status, and how many data bytes were written into
    /// the chain.
    Answer(u8, u32),
}

pub struct Blk {
    transport: Transport,
    backend: Box<dyn Backend>,
    slots: Vec<Slot>,
    /// A chain may be waiting on the ring for a slot: the device looks again
    /// when a buffer comes back.
    waiting: bool,
    /// A pass ended at its bound, with chains perhaps still on the ring: the
    /// next round goes on with them.
    more: bool,
    id: [u8; ID_BYTES],
    /// Set when the driver made a ring this device will not follow: no more
    /// requests are taken until it resets the device.
    broken: Option<Broken>,
    pub stats: Stats,
}

impl Blk {
    /// A disk of `backend`'s size -- whole sectors of it -- named `id` for
    /// the driver's GET_ID.
    pub fn new(backend: Box<dyn Backend>, id: &str) -> Result<Blk> {
        let mut queues = Vec::new();
        queues.try_reserve_exact(1).map_err(|_| Error::NoMemory)?;
        queues.push(Queue::new(QUEUE_SIZE));
        let mut slots = Vec::new();
        slots.try_reserve_exact(IN_FLIGHT).map_err(|_| Error::NoMemory)?;
        for _ in 0..IN_FLIGHT {
            let mut segs = Vec::new();
            segs.try_reserve_exact(usize::from(QUEUE_SIZE)).map_err(|_| Error::NoMemory)?;
            let mut buf = Vec::new();
            buf.try_reserve_exact(REQUEST_BYTES).map_err(|_| Error::NoMemory)?;
            buf.resize(REQUEST_BYTES, 0);
            slots.push(Slot { buf: Some(buf.into_boxed_slice()), head: None, segs });
        }
        let mut name = [0u8; ID_BYTES];
        let n = id.len().min(ID_BYTES);
        name[..n].copy_from_slice(&id.as_bytes()[..n]);
        /* A disk that may only be read has nothing to give back. */
        let ro = if backend.read_only() { F_RO } else { F_DISCARD | F_WRITE_ZEROES };
        Ok(Blk {
            transport: Transport::new(F_SIZE_MAX | F_SEG_MAX | F_FLUSH | ro, queues),
            backend,
            slots,
            waiting: false,
            more: false,
            id: name,
            broken: None,
            stats: Stats::default(),
        })
    }

    pub fn identity() -> Identity {
        Identity {
            vendor: virtio::VENDOR,
            device: virtio::LEGACY_DEVICE_BASE + VIRTIO_TYPE - 1,
            revision: 0,
            class: CLASS,
            subsystem_vendor: virtio::VENDOR,
            subsystem: VIRTIO_TYPE,
        }
    }

    /// Its size, in sectors.
    pub fn sectors(&self) -> u64 {
        self.backend.size() / SECTOR
    }

    /// What was wrong with the ring, if the device stopped over one.
    pub fn broken(&self) -> Option<Broken> {
        self.broken
    }

    /// Whether its queue has its rings copied (`Transport::rings_copied`).
    pub fn rings_copied(&self) -> bool {
        self.transport.rings_copied()
    }

    fn config(&self) -> [u8; CFG_BYTES] {
        let mut c = [0u8; CFG_BYTES];
        let at = usize::from(CFG_CAPACITY);
        c[at..at + 8].copy_from_slice(&self.sectors().to_le_bytes());
        let at = usize::from(CFG_SIZE_MAX);
        c[at..at + 4].copy_from_slice(&SIZE_MAX.to_le_bytes());
        let at = usize::from(CFG_SEG_MAX);
        c[at..at + 4].copy_from_slice(&SEG_MAX.to_le_bytes());
        for (at, value) in [(CFG_MAX_DISCARD_SECTORS, MAX_RANGE_SECTORS), (CFG_MAX_DISCARD_SEG, MAX_RANGES as u32),
                            (CFG_DISCARD_ALIGNMENT, DISCARD_ALIGNMENT), (CFG_MAX_ZEROES_SECTORS, MAX_RANGE_SECTORS),
                            (CFG_MAX_ZEROES_SEG, MAX_RANGES as u32)] {
            let at = usize::from(at);
            c[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }
        c[usize::from(CFG_ZEROES_MAY_UNMAP)] = 1;
        c
    }

    /// Offer MSI-X, with a table of `entries`.
    pub fn offer_msix(&mut self, entries: u16) -> Option<()> {
        self.transport.offer_msix(entries)
    }

    /// Whether the function has MSI-X on, as its capability says now.
    pub fn set_msix_enabled(&mut self, on: bool) {
        self.transport.set_msix_enabled(on);
    }

    /// Whether it asserts its INTx line now: a level, for an IO-APIC's pin.
    pub fn line(&self) -> bool {
        self.transport.line()
    }

    /// A read of `size` bytes at `offset` in the BAR.
    pub fn io_read(&mut self, offset: u16, size: u8) -> u32 {
        let config = self.transport.config_offset();
        if offset < config {
            return self.transport.read(offset, size);
        }
        let c = self.config();
        let mut v = 0u32;
        for i in 0..u16::from(size) {
            let at = usize::from(offset - config + i);
            if let Some(b) = c.get(at) {
                v |= u32::from(*b) << (8 * i);
            }
        }
        v
    }

    /// A write of `size` bytes at `offset` in the BAR, at `now`, and the
    /// interrupts it leaves the device with. The configuration is read-only.
    pub fn io_write(&mut self, offset: u16, size: u8, value: u32, mem: &GuestMemory, now: u64) -> Raise {
        if offset >= self.transport.config_offset() {
            return Raise::default();
        }
        match self.transport.write(offset, size, value, mem, now) {
            Asked::Notify(0) => {
                let answered = self.fill(mem);
                if answered { self.interrupt(mem) } else { Raise::default() }
            }
            Asked::Reset => {
                /* The chains with the backend are the old driver's: nothing
                 * is given back for them, and their buffers are taken back
                 * as they come. */
                for s in self.slots.iter_mut() {
                    s.head = None;
                }
                self.waiting = false;
                self.more = false;
                self.broken = None;
                Raise::default()
            }
            _ => Raise::default(),
        }
    }

    /// What the backend has served, given back to the guest -- a read's data
    /// into its buffers, the status, the chain on the used ring -- and what
    /// waited on the ring for a buffer, taken now there is one, or what a
    /// pass left at its bound -- and the interrupts that leaves the device
    /// with. Every time round the run loop, so it costs next to nothing when
    /// there is nothing.
    pub fn poll(&mut self, mem: &GuestMemory) -> Raise {
        let mut answered = false;
        let mut back = false;
        while let Some(req) = self.backend.take() {
            back = true;
            answered |= self.complete(mem, req);
        }
        if self.more || (back && self.waiting) {
            answered |= self.fill(mem);
        }
        if answered { self.interrupt(mem) } else { Raise::default() }
    }

    /// The interrupt, when the driver wants one: the line, when none is
    /// waiting already, or the queue's MSI-X vector.
    fn interrupt(&mut self, mem: &GuestMemory) -> Raise {
        let wants = self.transport.queue(0).map_or(false, |q| q.wants_interrupt(mem));
        if wants { self.transport.interrupt(0) } else { Raise::default() }
    }

    /// Take what the driver has made available, while there is a buffer for
    /// it and a pass's worth at most: each request handed to the backend, or
    /// -- when there is nothing for the backend to do -- answered at once.
    /// True when something was given back.
    fn fill(&mut self, mem: &GuestMemory) -> bool {
        self.waiting = false;
        self.more = false;
        let mut answered = false;
        let mut left = PASS;
        while self.broken.is_none() {
            if left == 0 {
                self.more = true;
                break;
            }
            left -= 1;
            let Some(slot) = self.slots.iter().position(|s| s.head.is_none() && s.buf.is_some()) else {
                /* Every buffer is with the backend: the rest waits on the
                 * ring until one comes back. */
                self.waiting = true;
                break;
            };
            let mut segs = core::mem::take(&mut self.slots[slot].segs);
            let popped = match self.transport.queue(0) {
                Some(q) if q.ready() => q.pop(mem, &mut segs),
                _ => Ok(None),
            };
            let result = match popped {
                Ok(Some(head)) => self.start(mem, slot, head, &segs),
                Ok(None) => {
                    self.slots[slot].segs = segs;
                    break;
                }
                Err(e) => Err(e),
            };
            self.slots[slot].segs = segs;
            match result {
                Ok(now) => answered |= now,
                Err(e) => {
                    self.broken = Some(e);
                    break;
                }
            }
        }
        answered
    }

    /// Serve the request at `head`, from `slot`: handed to the backend
    /// (false), or answered now (true).
    fn start(&mut self, mem: &GuestMemory, slot: usize, head: u16, segs: &[Seg])
        -> core::result::Result<bool, Broken>
    {
        let (op, offset, len, ranges) = match self.classify(mem, segs) {
            Next::Answer(status, written) => {
                self.finish(mem, segs, head, status, written)?;
                return Ok(true);
            }
            Next::Serve(op, offset, len) => (op, offset, len, None),
            Next::Ranges(op, count, ranges) => (op, 0, count * RANGE_BYTES, Some(ranges)),
        };
        let Some(mut buf) = self.slots[slot].buf.take() else {
            /* `fill` chose the slot for having its buffer. */
            self.stats.errors += 1;
            self.finish(mem, segs, head, S_IOERR, 0)?;
            return Ok(true);
        };
        if let Some(ranges) = ranges {
            /* The copy the ranges were checked in, not the guest's memory again */
            buf[..len].copy_from_slice(&ranges[..len]);
        }
        if op == Op::Write && !copy_out(mem, segs, HEADER as u64, &mut buf[..len]) {
            self.slots[slot].buf = Some(buf);
            self.stats.errors += 1;
            self.finish(mem, segs, head, S_IOERR, 0)?;
            return Ok(true);
        }
        self.slots[slot].head = Some(head);
        self.backend.submit(Request { op, offset, len, buf, ok: false, slot });
        Ok(false)
    }

    /// A request the backend has served, given back: its buffer to its slot
    /// whatever became of the chain, and the chain to the driver when there
    /// is still one to give it back to. True when something was.
    fn complete(&mut self, mem: &GuestMemory, req: Request) -> bool {
        let Request { op, len, buf, ok, slot, .. } = req;
        let Some(s) = self.slots.get_mut(slot) else {
            return false;
        };
        let head = s.head.take();
        let segs = core::mem::take(&mut s.segs);
        let answered = match head {
            Some(head) if self.broken.is_none() => {
                let (status, written) = if !ok {
                    (S_IOERR, 0)
                } else if op != Op::Read {
                    (S_OK, 0)
                } else if copy_in(mem, &segs, 0, &buf[..len]) {
                    (S_OK, clamp32(len as u64))
                } else {
                    (S_IOERR, 0)
                };
                if status == S_OK {
                    match op {
                        Op::Read => {
                            self.stats.reads += 1;
                            self.stats.read_bytes += len as u64;
                        }
                        Op::Write => {
                            self.stats.writes += 1;
                            self.stats.written_bytes += len as u64;
                        }
                        Op::Flush => self.stats.flushes += 1,
                        Op::Discard => {
                            self.stats.discards += 1;
                            self.stats.discarded_bytes += ranged_bytes(&buf[..len]);
                        }
                        Op::WriteZeroes => {
                            self.stats.zeroings += 1;
                            self.stats.zeroed_bytes += ranged_bytes(&buf[..len]);
                        }
                    }
                } else {
                    self.stats.errors += 1;
                }
                match self.finish(mem, &segs, head, status, written) {
                    Ok(()) => true,
                    Err(e) => {
                        self.broken = Some(e);
                        false
                    }
                }
            }
            /* A chain a reset took away, or a ring the device no longer
             * follows: nothing to give back to. */
            _ => false,
        };
        let s = &mut self.slots[slot];
        s.segs = segs;
        s.buf = Some(buf);
        answered
    }

    /// Write the status -- the last byte the device may write -- and give
    /// the chain back with what was written.
    fn finish(&mut self, mem: &GuestMemory, segs: &[Seg], head: u16, status: u8, written: u32) -> core::result::Result<(), Broken> {
        let last = segs.iter().rev().find(|s| s.write && s.len != 0).ok_or(Broken::Memory)?;
        let at = last.at(u64::from(last.len) - 1).ok_or(Broken::Memory)?;
        mem.write(at, &[status]).map_err(|_| Broken::Memory)?;
        let q = self.transport.queue(0).ok_or(Broken::Index)?;
        q.push(mem, head, written.saturating_add(1))
    }

    /// What a request is, from its header: the backend's to serve, or
    /// answered here -- a request past the disk, or larger than the driver
    /// was told one may be, is an I/O error; one with no data is done.
    fn classify(&mut self, mem: &GuestMemory, segs: &[Seg]) -> Next {
        let mut header = [0u8; HEADER];
        if !copy_out(mem, segs, 0, &mut header) {
            self.stats.errors += 1;
            return Next::Answer(S_IOERR, 0);
        }
        let kind = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
        let sector = u64::from_le_bytes([header[8], header[9], header[10], header[11],
                                         header[12], header[13], header[14], header[15]]);
        /* Everything the device may write but the status byte. */
        let writable: u64 = segs.iter().filter(|s| s.write).map(|s| u64::from(s.len)).sum();
        let room = writable.saturating_sub(1);
        let readable: u64 = segs.iter().filter(|s| !s.write).map(|s| u64::from(s.len)).sum();

        match kind {
            T_IN => self.data_request(Op::Read, sector, room),
            /* A driver that was told the disk is read-only does not write
             * to it; one that does anyway is told no. */
            T_OUT if self.backend.read_only() => {
                self.stats.errors += 1;
                Next::Answer(S_IOERR, 0)
            }
            T_OUT => self.data_request(Op::Write, sector, readable.saturating_sub(HEADER as u64)),
            T_FLUSH => {
                if self.transport.driver_features() & F_FLUSH != 0 {
                    Next::Serve(Op::Flush, 0, 0)
                } else {
                    self.stats.errors += 1;
                    Next::Answer(S_IOERR, 0)
                }
            }
            T_GET_ID => {
                let n = (room as usize).min(ID_BYTES);
                if copy_in(mem, segs, 0, &self.id[..n]) {
                    Next::Answer(S_OK, n as u32)
                } else {
                    Next::Answer(S_IOERR, 0)
                }
            }
            T_DISCARD => self.ranges_request(mem, segs, Op::Discard, F_DISCARD, readable),
            T_WRITE_ZEROES => self.ranges_request(mem, segs, Op::WriteZeroes, F_WRITE_ZEROES, readable),
            _ => Next::Answer(S_UNSUPP, 0),
        }
    }

    /// A discard or a write of zeros: its ranges copied out of the chain
    /// once, into the device's own copy, and checked there -- a count of
    /// them the driver was told it may send, each whole sectors inside the
    /// disk, no flag but a write of zeros' unmap. Unsupported unless the
    /// driver took the feature.
    fn ranges_request(&mut self, mem: &GuestMemory, segs: &[Seg], op: Op, feature: u32, readable: u64) -> Next {
        if self.transport.driver_features() & feature == 0 {
            return Next::Answer(S_UNSUPP, 0);
        }
        if self.backend.read_only() {
            self.stats.errors += 1;
            return Next::Answer(S_IOERR, 0);
        }
        let bytes = readable.saturating_sub(HEADER as u64);
        let count = (bytes / RANGE_BYTES as u64) as usize;
        if bytes % RANGE_BYTES as u64 != 0 || count == 0 || count > MAX_RANGES {
            self.stats.errors += 1;
            return Next::Answer(S_IOERR, 0);
        }
        let mut ranges = [0u8; MAX_RANGES * RANGE_BYTES];
        if !copy_out(mem, segs, HEADER as u64, &mut ranges[..count * RANGE_BYTES]) {
            self.stats.errors += 1;
            return Next::Answer(S_IOERR, 0);
        }
        let allowed = if op == Op::WriteZeroes { RANGE_UNMAP } else { 0 };
        for r in ranges[..count * RANGE_BYTES].chunks_exact(RANGE_BYTES) {
            let sector = u64::from_le_bytes([r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7]]);
            let sectors = u32::from_le_bytes([r[8], r[9], r[10], r[11]]);
            let flags = u32::from_le_bytes([r[12], r[13], r[14], r[15]]);
            if flags & !allowed != 0 {
                return Next::Answer(S_UNSUPP, 0);
            }
            if sectors > MAX_RANGE_SECTORS || self.range(sector, u64::from(sectors) * SECTOR).is_none() {
                self.stats.errors += 1;
                return Next::Answer(S_IOERR, 0);
            }
        }
        Next::Ranges(op, count, ranges)
    }

    /// A read or a write of `len` bytes from `sector`.
    fn data_request(&mut self, op: Op, sector: u64, len: u64) -> Next {
        match self.range(sector, len) {
            Some(_) if len == 0 => Next::Answer(S_OK, 0),
            Some(offset) if len <= REQUEST_BYTES as u64 => Next::Serve(op, offset, len as usize),
            _ => {
                self.stats.errors += 1;
                Next::Answer(S_IOERR, 0)
            }
        }
    }

    /// The byte offset of `sector`, when `len` bytes from there are whole
    /// sectors within the disk.
    fn range(&self, sector: u64, len: u64) -> Option<u64> {
        let offset = sector.checked_mul(SECTOR)?;
        let end = offset.checked_add(len)?;
        (len % SECTOR == 0 && end <= self.backend.size()).then_some(offset)
    }
}

/// A count of bytes, as the used ring's 32 bits hold it.
fn clamp32(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The bytes ranges cover, for the stats.
fn ranged_bytes(ranges: &[u8]) -> u64 {
    ranges.chunks_exact(RANGE_BYTES)
        .map(|r| u64::from(u32::from_le_bytes([r[8], r[9], r[10], r[11]])) * SECTOR)
        .sum()
}

/// Copy `buf.len()` bytes of the chain's readable stream, from `skip` bytes
/// into it, out of guest memory.
fn copy_out(mem: &GuestMemory, segs: &[Seg], skip: u64, buf: &mut [u8]) -> bool {
    let mut skip = skip;
    let mut done = 0usize;
    for s in segs.iter().filter(|s| !s.write) {
        if done == buf.len() {
            break;
        }
        let len = u64::from(s.len);
        if skip >= len {
            skip -= len;
            continue;
        }
        let n = ((len - skip) as usize).min(buf.len() - done);
        let Some(at) = s.at(skip) else { return false };
        if mem.read(at, &mut buf[done..done + n]).is_err() {
            return false;
        }
        done += n;
        skip = 0;
    }
    done == buf.len()
}

/// Copy `data` into the chain's writable stream, from `skip` bytes into it.
fn copy_in(mem: &GuestMemory, segs: &[Seg], skip: u64, data: &[u8]) -> bool {
    let mut skip = skip;
    let mut done = 0usize;
    for s in segs.iter().filter(|s| s.write) {
        if done == data.len() {
            break;
        }
        let len = u64::from(s.len);
        if skip >= len {
            skip -= len;
            continue;
        }
        let n = ((len - skip) as usize).min(data.len() - done);
        let Some(at) = s.at(skip) else { return false };
        if mem.write(at, &data[done..done + n]).is_err() {
            return false;
        }
        done += n;
        skip = 0;
    }
    done == data.len()
}
