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
//! device's -- and hands each request to the backend, which serves them in
//! order while the guest runs on; each time round the run loop (`poll`) what
//! the backend has served is given back: a read's data copied into the
//! guest, the status written, the chain on the used ring, and an interrupt.
//! The guest's memory is only ever touched here, on the vCPU's task: the
//! backend sees buffers of the device's and nothing else.
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
use super::virtio::{self, Asked, Broken, Queue, Seg, Transport};
use crate::memory::GuestMemory;
use crate::{Error, Result};

/// What stores a disk's bytes, and serves the requests the device hands it
/// -- in the order it is handed them, beside the guest, from a task of its
/// own or however it likes -- until `take` gives each back.
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
    /// A request served, if one is: in the order they were submitted.
    fn take(&mut self) -> Option<Request>;
}

/// What a request asks of the disk.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Read,
    Write,
    /// Everything written so far, on stable storage.
    Flush,
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
}

/// Virtio's type for a block device.
const VIRTIO_TYPE: u16 = 2;
/// Mass storage, SCSI -- what a legacy virtio disk has always said it is.
const CLASS: u32 = 0x01_00_00;
/// Its I/O BAR: the header and the configuration below, rounded up.
pub const BAR_SIZE: u32 = 0x40;

pub const SECTOR: u64 = 512;
/// The queue's size. A request takes a descriptor a segment plus two, so a
/// ring this size holds three of the largest at once -- and the device has
/// one on its way back while the backend serves the next.
const QUEUE_SIZE: u16 = 256;

/// Features: a limit on a segment's size and on the segments a request may
/// have, flush, and a disk that is read-only.
const F_SIZE_MAX: u32 = 1 << 1;
const F_SEG_MAX: u32 = 1 << 2;
const F_RO: u32 = 1 << 5;
const F_FLUSH: u32 = 1 << 9;
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
const CFG_BYTES: usize = 24;

/// What the device has done, for a report.
#[derive(Clone, Copy, Default)]
pub struct Stats {
    pub reads: u64,
    pub writes: u64,
    pub flushes: u64,
    pub read_bytes: u64,
    pub written_bytes: u64,
    pub errors: u64,
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
        let ro = if backend.read_only() { F_RO } else { 0 };
        Ok(Blk {
            transport: Transport::new(F_SIZE_MAX | F_SEG_MAX | F_FLUSH | ro, queues),
            backend,
            slots,
            waiting: false,
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

    fn config(&self) -> [u8; CFG_BYTES] {
        let mut c = [0u8; CFG_BYTES];
        let at = usize::from(CFG_CAPACITY);
        c[at..at + 8].copy_from_slice(&self.sectors().to_le_bytes());
        let at = usize::from(CFG_SIZE_MAX);
        c[at..at + 4].copy_from_slice(&SIZE_MAX.to_le_bytes());
        let at = usize::from(CFG_SEG_MAX);
        c[at..at + 4].copy_from_slice(&SEG_MAX.to_le_bytes());
        c
    }

    /// A read of `size` bytes at `offset` in the BAR.
    pub fn io_read(&mut self, offset: u16, size: u8) -> u32 {
        if offset < virtio::DEVICE_CONFIG {
            return self.transport.read(offset, size);
        }
        let c = self.config();
        let mut v = 0u32;
        for i in 0..u16::from(size) {
            let at = usize::from(offset - virtio::DEVICE_CONFIG + i);
            if let Some(b) = c.get(at) {
                v |= u32::from(*b) << (8 * i);
            }
        }
        v
    }

    /// A write of `size` bytes at `offset` in the BAR; true when the
    /// device's interrupt line is to go up. The configuration is read-only.
    pub fn io_write(&mut self, offset: u16, size: u8, value: u32, mem: &mut GuestMemory) -> bool {
        if offset >= virtio::DEVICE_CONFIG {
            return false;
        }
        match self.transport.write(offset, size, value) {
            Asked::Notify(0) => {
                let answered = self.fill(mem);
                answered && self.interrupt(mem)
            }
            Asked::Reset => {
                /* The chains with the backend are the old driver's: nothing
                 * is given back for them, and their buffers are taken back
                 * as they come. */
                for s in self.slots.iter_mut() {
                    s.head = None;
                }
                self.waiting = false;
                self.broken = None;
                false
            }
            _ => false,
        }
    }

    /// What the backend has served, given back to the guest -- a read's data
    /// into its buffers, the status, the chain on the used ring -- and what
    /// waited on the ring for a buffer, taken now there is one. True when the
    /// device's interrupt line is to go up. Every time round the run loop,
    /// so it costs next to nothing when there is nothing.
    pub fn poll(&mut self, mem: &mut GuestMemory) -> bool {
        let mut answered = false;
        let mut back = false;
        while let Some(req) = self.backend.take() {
            back = true;
            answered |= self.complete(mem, req);
        }
        if back && self.waiting {
            answered |= self.fill(mem);
        }
        answered && self.interrupt(mem)
    }

    /// The interrupt, when the driver wants one and none is waiting already.
    fn interrupt(&mut self, mem: &GuestMemory) -> bool {
        let wants = self.transport.queue(0).map_or(false, |q| q.wants_interrupt(mem));
        wants && self.transport.interrupt()
    }

    /// Take what the driver has made available, while there is a buffer for
    /// it: each request handed to the backend, or -- when there is nothing
    /// for the backend to do -- answered at once. True when something was
    /// given back.
    fn fill(&mut self, mem: &mut GuestMemory) -> bool {
        self.waiting = false;
        let mut answered = false;
        while self.broken.is_none() {
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
    fn start(&mut self, mem: &mut GuestMemory, slot: usize, head: u16, segs: &[Seg])
        -> core::result::Result<bool, Broken>
    {
        let (op, offset, len) = match self.classify(mem, segs) {
            Next::Answer(status, written) => {
                self.finish(mem, segs, head, status, written)?;
                return Ok(true);
            }
            Next::Serve(op, offset, len) => (op, offset, len),
        };
        let Some(mut buf) = self.slots[slot].buf.take() else {
            /* `fill` chose the slot for having its buffer. */
            self.stats.errors += 1;
            self.finish(mem, segs, head, S_IOERR, 0)?;
            return Ok(true);
        };
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
    fn complete(&mut self, mem: &mut GuestMemory, req: Request) -> bool {
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
    fn finish(&mut self, mem: &mut GuestMemory, segs: &[Seg], head: u16, status: u8, written: u32) -> core::result::Result<(), Broken> {
        let last = segs.iter().rev().find(|s| s.write && s.len != 0).ok_or(Broken::Memory)?;
        mem.write(last.addr + u64::from(last.len) - 1, &[status]).map_err(|_| Broken::Memory)?;
        let q = self.transport.queue(0).ok_or(Broken::Index)?;
        q.push(mem, head, written.saturating_add(1))
    }

    /// What a request is, from its header: the backend's to serve, or
    /// answered here -- a request past the disk, or larger than the driver
    /// was told one may be, is an I/O error; one with no data is done.
    fn classify(&mut self, mem: &mut GuestMemory, segs: &[Seg]) -> Next {
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
            _ => Next::Answer(S_UNSUPP, 0),
        }
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
        if mem.read(s.addr + skip, &mut buf[done..done + n]).is_err() {
            return false;
        }
        done += n;
        skip = 0;
    }
    done == buf.len()
}

/// Copy `data` into the chain's writable stream, from `skip` bytes into it.
fn copy_in(mem: &mut GuestMemory, segs: &[Seg], skip: u64, data: &[u8]) -> bool {
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
        if mem.write(s.addr + skip, &data[done..done + n]).is_err() {
            return false;
        }
        done += n;
        skip = 0;
    }
    done == data.len()
}
