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
//! the data of a write copied out of guest memory then, into the request's
//! buffer -- and hands each request to the backend, which serves them
//! while the guest runs on: in whatever order it likes, several at once if
//! it can -- virtio orders nothing between requests in flight; a driver
//! that needs a write done before another waits for it, and before a
//! flush. Each time round the run loop (`poll`) what the backend has served
//! is given back: a read's data copied into the guest, the status written,
//! the chain on the used ring, and an interrupt -- and so does the disk's
//! own task as it serves something (`LinuxGuest::serve_disk`), taking more
//! off the ring as it goes. The guest's memory is touched only here, under
//! the platform's lock, by whichever task holds it -- a vCPU's or the
//! disk's: the lock, not the task, is what orders it against a reset and
//! every other access. The backend sees its own buffers and nothing else.
//! They are the backend's -- one a request it may be handed, which the
//! device reaches by number (`Backend::with_buffer`) while the request is
//! not with the backend -- so that a backend can have its device read and
//! write them as they are, with no copy of its own.
//!
//! A reset, or the queue set up anew, forgets the chains the backend still
//! has requests of: they are the old driver's, given back to nobody. And
//! the new driver's requests are not started until those are back: served
//! beside them, an old write and a new one to the same blocks could land in
//! either order -- a crash kernel's journal under its dead kernel's last
//! writes, over a backend that stalled (QEMU drains its backend at a reset
//! for this). A driver that takes no FLUSH may take its disk to be
//! write-through (virtio 1.x, 5.2.5.2): each write of one is flushed before
//! it is answered, as QEMU puts its backend in writethrough mode for one.
//!
//! A discard and a write of zeros come as ranges -- sectors, a count, and
//! for a write of zeros whether the blocks may be given back -- which the
//! device copies out of the guest's memory once, checks there, and hands
//! the backend that copy (`Request::ranges`): a guest that changes them
//! after the check changes nothing it was checked on.
//!
//! At most `IN_FLIGHT` requests are with the backend at once, each in a
//! buffer of `REQUEST_BYTES` the backend has from the start -- what a driver
//! that takes `SEG_MAX` and `SIZE_MAX` is told a request may carry. One that
//! does not (EDK2's VirtioBlkDxe, which puts a whole read in one descriptor)
//! has its request served a buffer at a time, with one status at its end.
//! Nothing is allocated on the way, and a guest that keeps its ring full
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
    /// The sectors a discard is best aligned to and best of at least: what
    /// the backend gives back whole, which the device tells the driver --
    /// Linux then splits a discard on those boundaries, and leaves out free
    /// runs shorter. `DISCARD_ALIGNMENT` unless the backend says otherwise;
    /// never more than a discard's range takes.
    fn discard_sectors(&self) -> u32 {
        DISCARD_ALIGNMENT
    }
    /// Whether the backend takes a discard (`Op::Discard`) -- which the
    /// device offers its driver only then -- and a write of zeros
    /// (`Op::WriteZeroes`): both, unless it says not.
    fn discards(&self) -> bool {
        true
    }
    fn writes_zeroes(&self) -> bool {
        true
    }
    /// The bytes of the backend's block -- `SECTOR`, or a power of two of
    /// them up to a page -- which every request it is handed is whole ones
    /// of and starts on. The guest is shown sectors still, the block as the
    /// disk's physical one (`F_TOPOLOGY`): a partition table, a filesystem
    /// laid out in sectors -- every distribution's image -- is read as it
    /// was written. A request of part of a block is the device's to serve
    /// in whole ones (`Pad`). `SECTOR` unless the backend says otherwise.
    fn block_size(&self) -> u32 {
        SECTOR as u32
    }
    /// Buffer `slot`'s bytes, at least `REQUEST_BYTES` of them, for `use_it`
    /// -- what a write's data is copied into before its request is handed
    /// over, and a read's taken from once it is back: its result, or false
    /// where the backend has no such buffer, or has the request of it. One
    /// buffer a slot, `0..IN_FLIGHT`, the backend's for as long as it is.
    fn with_buffer(&mut self, slot: usize, use_it: &mut dyn FnMut(&mut [u8]) -> bool) -> bool;
    /// Serve `req`, whose data is in its slot's buffer. The device never has
    /// more than `IN_FLIGHT` handed over and not yet taken back -- one a
    /// slot -- and a backend takes that many.
    fn submit(&mut self, req: Request);
    /// A request served, if one is: in the order they were served, which
    /// need not be the order they were submitted in.
    fn take(&mut self) -> Option<Request>;
    /// The volume of the cluster's this disk is, and its writer's generation
    /// -- what a migration's handover takes it from (plans/vm-migration.md).
    /// None for a disk of the host's own.
    fn writer(&self) -> Option<(&str, u64)> {
        None
    }
}

/// A disk that stands in for one not had yet -- a migration's new host's,
/// until the handover gives it the volume (`Blk::set_backend`): the guest
/// sees its size, its block and what it takes, as it saw the disk on the old
/// host, and nothing is ever asked of it -- the guest does not run until
/// the disk is had. A request handed it all the same is an error the guest
/// is told of, not a hang.
pub struct Standin {
    pub bytes: u64,
    pub block: u32,
    pub read_only: bool,
    pub discards: bool,
    pub writes_zeroes: bool,
    done: alloc::collections::VecDeque<Request>,
}

impl Standin {
    pub fn new(bytes: u64, block: u32, read_only: bool, discards: bool, writes_zeroes: bool) -> Option<Standin> {
        let mut done = alloc::collections::VecDeque::new();
        done.try_reserve_exact(IN_FLIGHT).ok()?;
        Some(Standin { bytes, block, read_only, discards, writes_zeroes, done })
    }
}

impl Backend for Standin {
    fn size(&self) -> u64 {
        self.bytes
    }
    fn read_only(&self) -> bool {
        self.read_only
    }
    fn discards(&self) -> bool {
        self.discards
    }
    fn writes_zeroes(&self) -> bool {
        self.writes_zeroes
    }
    fn block_size(&self) -> u32 {
        self.block
    }
    fn with_buffer(&mut self, _slot: usize, _use_it: &mut dyn FnMut(&mut [u8]) -> bool) -> bool {
        false
    }
    fn submit(&mut self, mut req: Request) {
        req.done(false);
        /* Into the room taken at `new`: the device never has more out. */
        if self.done.len() < IN_FLIGHT {
            self.done.push_back(req);
        }
    }
    fn take(&mut self) -> Option<Request> {
        self.done.pop_front()
    }
}

/// A disk as its guest sees it (`Blk::shape`): what a stand-in of it is
/// made to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shape {
    pub bytes: u64,
    pub block: u32,
    pub read_only: bool,
    pub discards: bool,
    pub writes_zeroes: bool,
    /// What its GET_ID says, NUL-padded.
    pub id: [u8; ID_BYTES],
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

/// One request, on its way to the backend and back: the device's -- a
/// backend serves one it was handed and gives it back, and has no way to
/// make one of its own. Its data is in its slot's buffer, the backend's.
pub struct Request {
    op: Op,
    offset: u64,
    len: usize,
    ok: bool,
    slot: usize,
    /// A discard's or a write of zeros' ranges, as the device checked them:
    /// `len` bytes of them.
    ranges: [u8; MAX_RANGES * RANGE_BYTES],
}

impl Request {
    pub fn op(&self) -> Op {
        self.op
    }

    /// Where on the disk, in bytes: whole blocks of the backend's, inside it
    /// (a read or a write).
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// A read's or a write's bytes: whole blocks of the backend's, at most
    /// `REQUEST_BYTES`, from the start of its buffer.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Which buffer of the backend's its data is in (`Backend::with_buffer`).
    pub fn slot(&self) -> usize {
        self.slot
    }

    /// How it went: false for an error the guest is to see.
    pub fn done(&mut self, ok: bool) {
        self.ok = ok;
    }

    /// A discard's or a write of zeros' ranges, as the device checked them
    /// in its own copy: every one whole sectors inside the disk. None for
    /// any other request.
    pub fn ranges(&self) -> impl Iterator<Item = Range> + '_ {
        let ranges = if matches!(self.op, Op::Discard | Op::WriteZeroes) { &self.ranges[..self.len] } else { &[] };
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
/// The queue's size. A request takes one descriptor of it, its buffers in a
/// table of their own (`virtio::F_INDIRECT_DESC`) -- so a ring this size
/// holds every request the device takes at once and as many again waiting.
/// Without indirect descriptors a request takes a descriptor a segment plus
/// two: three of the largest at once, 768 KiB -- all a guest writing its
/// disk in megabytes had with it, whatever its queue's depth.
const QUEUE_SIZE: u16 = 256;
/// The most chains taken off the queue in one pass: a queue's worth, all a
/// driver can have available at once. A request answered here -- a GET_ID,
/// one past the disk -- takes no buffer, so running out of buffers does not
/// end a pass of them; unbounded, a guest that kept the ring topped up with
/// such requests from a second CPU held the CPU running the pass in the host
/// for good (hv-fuzz's racing CPU). What is left is the next round's.
const PASS: usize = QUEUE_SIZE as usize;

/// How many requests with the backend let the device leave its driver
/// unasked for a kick, the ring looked at again as each comes back
/// (`Blk::fill`): at a disk's 38 us for a 4k read, eight out bring one back
/// every 5 us at most, the longest a request added meanwhile waits on the
/// ring. Below it -- a job at QD1 to QD4 -- every request is a kick, as
/// without.
const KICKLESS_OUT: usize = 8;

/// Features: a limit on a segment's size and on the segments a request may
/// have, flush, a disk that is read-only, and -- one that is not -- discard
/// and a write of zeros; a backend's block larger than a sector as the
/// disk's physical block, its sectors still `SECTOR` (`F_TOPOLOGY`, never
/// `F_BLK_SIZE`, which would make the block the guest's sector); and the
/// ring's EVENT_IDX, the device asking for a kick only when it has taken
/// every chain there was, and none while its buffers are all with the
/// backend -- a buffer coming back has it look -- and its indirect
/// descriptors (`QUEUE_SIZE`).
const F_SIZE_MAX: u32 = 1 << 1;
const F_SEG_MAX: u32 = 1 << 2;
const F_RO: u32 = 1 << 5;
const F_TOPOLOGY: u32 = 1 << 10;
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
/// The sectors a discard is best aligned to unless the backend says
/// otherwise (`Backend::discard_sectors`): 4 KiB, a block.
pub const DISCARD_ALIGNMENT: u32 = 8;
/// What a request may carry, which the driver is told: data segments of at
/// most a page, and this many of them -- two fewer where the backend's
/// block is larger than a sector, so that a request and the parts of the
/// blocks at either end of it (`Pad`) fit a buffer still.
const SIZE_MAX: u32 = 4096;
const SEG_MAX: u32 = 64;
const PAD_SEGS: u32 = 2;
/// The most data one request handed to the backend has, and so the size of
/// a buffer. A guest's larger request is served in parts of this size.
pub const REQUEST_BYTES: usize = (SIZE_MAX * SEG_MAX) as usize;
/// The most data a pass moves between the guest's memory and the backend's
/// buffers -- a write's taken out, a read's given back -- before it ends at
/// its bound (`more`), the next round going on with the rest. A pass runs
/// under the platform's lock, which every other CPU's port and MMIO access
/// waits for: unbounded, a guest writing a megabyte at a time eight deep
/// had all of it copied under one -- 8 MiB -- and Linux's clocksource
/// watchdog on another of its CPUs found a read of the PM timer taking
/// 800 us. Two of the largest requests: a pass of tens of microseconds, and
/// always one request at least.
pub const PASS_BYTES: usize = 2 * REQUEST_BYTES;

/// The most requests with the backend at once, and so how many buffers the
/// backend has: 16 MiB of them. 64, as much as a guest's four jobs of 16
/// random reads each keep out -- under KVM 217k IOPS where 32 of them made
/// 195k -- which the NVMe driver's queue has room for besides everyone
/// else's. At 8, a guest's 4 KiB reads at QD32 were 8 at the device, each
/// about 100 us: the 80k IOPS the EX44 measured where KVM did 195k.
pub const IN_FLIGHT: usize = 64;

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
const CFG_BLK_SIZE: u16 = 20;
const CFG_PHYSICAL_BLOCK_EXP: u16 = 24;
const CFG_MIN_IO_SIZE: u16 = 26;
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
    /// The driver's notifies -- each an exit -- and the interrupts raised
    /// for it: what EVENT_IDX spares both of.
    pub notifies: u64,
    pub interrupts: u64,
    /// Passes ended at their bound on the data they moved (`PASS_BYTES`).
    pub cut: u64,
    /// What the backend took over the requests answered, from the hand-off
    /// to the answer -- a flush behind a write counted with the write -- in
    /// nanoseconds, each request's at most 2^32 of them as the trace's
    /// records clamp a time (`tp::ns`): a client's latency is its growth
    /// over the requests' between two looks. Two reads of the cycle counter
    /// a request.
    pub wait_ns: u64,
}

/// A request with the backend, as the device keeps it: the chain it came in
/// on -- its head, and its buffers, where a read's data goes and the status
/// after it. What it asked comes back with the request.
struct Slot {
    /// Whether the backend has the slot's request, and its buffer.
    out: bool,
    /// The chain being served, until it is given back. None when the slot is
    /// free -- or when a reset, or the queue set up anew, forgot its chain
    /// while the backend still has the request, which is then taken back and
    /// given to nobody.
    head: Option<u16>,
    /// The chain's buffers: room for the longest chain the queue holds.
    segs: Vec<Seg>,
    /// A write the slot's request is the flush behind, for a driver that
    /// took no FLUSH: what it was -- its op, its bytes, a discard's or a
    /// write of zeros' ranges -- answered for when the flush comes back.
    behind: Option<(Op, usize, [u8; MAX_RANGES * RANGE_BYTES])>,
    /// The cycle counter when its request went to the backend: what its
    /// wait is counted from (`Stats::wait_ns`).
    at: u64,
    /// Whether the request with the backend changes what the disk holds: a
    /// write of part of a block waits for every one to come back.
    wrote: bool,
    /// A read or a write of part of a block, served in whole ones: what the
    /// guest asked of the blocks handed over.
    pad: Option<Pad>,
    /// Where a guest's read or write larger than a backend buffer has got:
    /// its parts share the slot and its chain, and only the last is answered.
    part: Option<Part>,
}

/// A guest's read or write being served a backend buffer at a time. `next`
/// and `left` name what is still to do after the request out; `done` is where
/// its data goes in the guest's chain.
#[derive(Clone, Copy)]
struct Part {
    op: Op,
    next: u64,
    left: usize,
    done: usize,
}

/// A read or a write of whole sectors that are not whole blocks of the
/// backend's, served in the blocks around it -- the span. A read of them
/// gives the guest its part; a write reads them first, merges the guest's
/// data in and writes them whole, and while it does no other write is
/// handed over (`Blk::rmw`): beside it, a write of another part of the same
/// block -- or of the whole of it -- would be undone by its writing back
/// what it read before.
#[derive(Clone, Copy)]
struct Pad {
    /// The guest's bytes: `len` of them, `skip` into the span.
    skip: usize,
    len: usize,
    /// A write's span being read, its data not merged in yet.
    reading: bool,
}

/// A request taken off the ring that has to wait for the writes with the
/// backend -- a write of part of a block for every one of them, any other
/// write for such a write -- as it was checked, its chain in its slot.
#[derive(Clone, Copy)]
struct Parked {
    slot: usize,
    head: u16,
    op: Op,
    offset: u64,
    len: usize,
    ranges: [u8; MAX_RANGES * RANGE_BYTES],
}

impl Slot {
    /// Holds a request a reset, or the queue set up anew, took the chain of
    /// away: the old driver's, still with the backend.
    fn orphan(&self) -> bool {
        self.out && self.head.is_none()
    }
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
    /// The ring was found empty with KICKLESS_OUT requests or more with the
    /// backend, and no kick asked for: the device looks at the ring again at
    /// every request that comes back instead (`poll`), until it finds fewer
    /// out and asks for one.
    kickless: bool,
    /// A pass ended at its bound, with chains perhaps still on the ring, or
    /// requests the backend served still with it: the next round goes on
    /// with them.
    more: bool,
    /// The data the pass under way has moved so far (`PASS_BYTES`).
    moved: usize,
    id: [u8; ID_BYTES],
    /// Its number among its guest's disks, vda 0: what its requests' trace
    /// records say it by (`crate::tp`).
    index: u8,
    /// Set when the driver made a ring this device will not follow: no more
    /// requests are taken until it resets the device.
    broken: Option<Broken>,
    /// A write of part of a block is with the backend -- its span being
    /// read, or written back: no other write is handed over until it is
    /// back (`Pad`).
    rmw: bool,
    /// A request that waits for the writes with the backend, nothing more
    /// taken off the ring behind it: started first as they come back.
    parked: Option<Parked>,
    /// Nothing more taken off the ring (`quiesce`): what is with the backend
    /// comes back and is given to the guest, and a request parked behind
    /// writes is still started -- a VM paused for its state, being drained.
    quiesced: bool,
    pub stats: Stats,
}

impl Blk {
    /// A disk of `backend`'s size -- whole sectors of it -- named `id` for
    /// the driver's GET_ID, its guest's disk `index` (vda 0).
    pub fn new(backend: Box<dyn Backend>, id: &str, index: u8) -> Result<Blk> {
        let mut queues = Vec::new();
        queues.try_reserve_exact(1).map_err(|_| Error::NoMemory)?;
        queues.push(Queue::new(QUEUE_SIZE));
        let mut slots = Vec::new();
        slots.try_reserve_exact(IN_FLIGHT).map_err(|_| Error::NoMemory)?;
        for _ in 0..IN_FLIGHT {
            let mut segs = Vec::new();
            segs.try_reserve_exact(usize::from(QUEUE_SIZE)).map_err(|_| Error::NoMemory)?;
            slots.push(Slot { out: false, head: None, segs, behind: None, at: 0, wrote: false, pad: None, part: None });
        }
        let mut name = [0u8; ID_BYTES];
        let n = id.len().min(ID_BYTES);
        name[..n].copy_from_slice(&id.as_bytes()[..n]);
        /* A disk that may only be read has nothing to give back; one that
         * may be written, what its backend takes of the two -- a write of
         * zeros only where a block is a sector: of part of one it would be
         * a read, a merge and a write of its own. */
        Ok(Blk {
            transport: Transport::new(Self::features_of(&*backend), queues),
            backend,
            slots,
            waiting: false,
            kickless: false,
            more: false,
            moved: 0,
            id: name,
            index,
            broken: None,
            rmw: false,
            parked: None,
            quiesced: false,
            stats: Stats::default(),
        })
    }

    /// The features a device over `backend` offers: what a backend that
    /// takes its place must have the device offer alike (`set_backend`).
    fn features_of(backend: &dyn Backend) -> u32 {
        let bs = block_bytes(backend);
        let ro = if backend.read_only() {
            F_RO
        } else {
            (if backend.discards() { F_DISCARD } else { 0 })
                | (if backend.writes_zeroes() && bs == SECTOR { F_WRITE_ZEROES } else { 0 })
        };
        let topology = if bs != SECTOR { F_TOPOLOGY } else { 0 };
        F_SIZE_MAX | F_SEG_MAX | F_FLUSH | virtio::F_EVENT_IDX | virtio::F_INDIRECT_DESC | ro | topology
    }

    /// Stop taking chains off the ring, or take them again: a VM paused for
    /// its state is drained by the first -- what is with the backend still
    /// comes back and is given to the guest, a request parked behind writes
    /// still started -- and one that goes on after all by the second, which
    /// has the next round look at the ring.
    pub fn quiesce(&mut self, on: bool) {
        self.quiesced = on;
        if !on {
            self.more = true;
        }
    }

    /// The ranges its queues' rings are in (`Transport::rings`).
    pub fn rings(&self, out: &mut dyn FnMut(u64, u64)) {
        self.transport.rings(out);
    }

    /// The disk as its guest sees it: what a stand-in for it is made to be.
    pub fn shape(&self) -> Shape {
        let b = &*self.backend;
        Shape { bytes: self.disk_bytes(), block: self.block() as u32, read_only: b.read_only(),
                discards: b.discards(), writes_zeroes: b.writes_zeroes(), id: self.id }
    }

    /// Its backend's volume and writer's generation (`Backend::writer`).
    pub fn writer(&self) -> Option<(&str, u64)> {
        self.backend.writer()
    }

    /// Whether nothing of the guest's is with the backend, or waits for it:
    /// what its state may be read at.
    pub fn drained(&self) -> bool {
        self.parked.is_none() && !self.rmw && self.slots.iter().all(|s| !s.out && s.head.is_none())
    }

    /// What the driver made of it, for a VM's state (`vmstate`) -- drained
    /// first: a disk with a request in flight has a state no other host
    /// could go on from.
    pub fn save(&self) -> vmstate::Result<vmstate::Blk> {
        if !self.drained() {
            return Err(vmstate::Error("a disk with requests in flight"));
        }
        let transport = self.transport.save().ok_or(vmstate::Error("no memory for a disk's state"))?;
        Ok(vmstate::Blk { transport, broken: self.broken.map(Broken::code) })
    }

    /// Made what `s` says, on a device built over a disk the guest sees as
    /// it saw the one it was saved over -- the same features, the same queue
    /// -- its rings set up again in `mem` at `now`. Its first round looks at
    /// the ring: what the driver made available and the device had not
    /// taken is taken then.
    pub fn restore(&mut self, s: &vmstate::Blk, mem: &GuestMemory, now: u64) -> vmstate::Result<()> {
        if !self.drained() {
            return Err(vmstate::Error("a disk restored with requests in flight"));
        }
        let broken = match s.broken {
            None => None,
            Some(c) => Some(Broken::from_code(c).ok_or(vmstate::Error("a ring broken in no way there is"))?),
        };
        self.transport.restore(&s.transport, mem, now)?;
        self.broken = broken;
        self.waiting = false;
        self.kickless = false;
        self.more = true;
        self.moved = 0;
        self.quiesced = false;
        Ok(())
    }

    /// `backend` in place of the one it was built over -- one that stood in
    /// for the disk until it could be had, a migration's handover -- with
    /// nothing of the guest's with that one: given back. Refused, and
    /// handed back, unless the guest sees the disk alike through it: the
    /// same size, the same block, the same features.
    pub fn set_backend(&mut self, backend: Box<dyn Backend>) -> core::result::Result<Box<dyn Backend>, Box<dyn Backend>> {
        let same = self.drained()
            && Self::features_of(&*backend) == Self::features_of(&*self.backend)
            && block_bytes(&*backend) == self.block()
            && backend.size() / self.block() * self.block() == self.disk_bytes();
        if !same {
            return Err(backend);
        }
        Ok(core::mem::replace(&mut self.backend, backend))
    }

    pub fn identity() -> Identity {
        Identity {
            vendor: virtio::VENDOR,
            device: virtio::LEGACY_BLK,
            revision: 0,
            class: CLASS,
            subsystem_vendor: virtio::VENDOR,
            subsystem: VIRTIO_TYPE,
        }
    }

    /// Its size, in sectors.
    pub fn sectors(&self) -> u64 {
        self.disk_bytes() / SECTOR
    }

    /// The backend's block, in bytes (`Backend::block_size`).
    fn block(&self) -> u64 {
        block_bytes(&*self.backend)
    }

    /// The disk's bytes the guest is shown: whole blocks of the backend's,
    /// so that the blocks around any sector of it are inside it.
    fn disk_bytes(&self) -> u64 {
        let bs = self.block();
        self.backend.size() / bs * bs
    }

    /// The segments a request may have, which the driver is told
    /// (`PAD_SEGS`).
    fn seg_max(&self) -> u32 {
        if self.block() == SECTOR { SEG_MAX } else { SEG_MAX - PAD_SEGS }
    }

    /// The blocks of the backend's a read or a write of `len` bytes at
    /// `offset` lies in -- where they start, and their bytes -- or the
    /// request itself, where it is whole blocks or neither.
    fn span(&self, op: Op, offset: u64, len: usize) -> (u64, usize) {
        let bs = self.block();
        if !matches!(op, Op::Read | Op::Write) || bs == SECTOR {
            return (offset, len);
        }
        /* Inside the disk, its bytes whole blocks (`range`, `disk_bytes`) */
        let at = offset / bs * bs;
        let end = offset.saturating_add(len as u64).div_ceil(bs).saturating_mul(bs);
        (at, end.saturating_sub(at) as usize)
    }

    /// Whether a request may be handed over now: a read or a flush always;
    /// no write while a write of part of a block is out; a write of part of
    /// a block only with no write out at all (`Pad`).
    fn may_start(&self, op: Op, offset: u64, len: usize) -> bool {
        if !op.writes() {
            return true;
        }
        if self.rmw {
            return false;
        }
        let padded = op == Op::Write && self.span(op, offset, len) != (offset, len);
        !padded || !self.slots.iter().any(|s| s.out && s.wrote)
    }

    /// Its number among its guest's disks, vda 0.
    pub fn index(&self) -> u8 {
        self.index
    }

    /// What was wrong with the ring, if the device stopped over one.
    pub fn broken(&self) -> Option<Broken> {
        self.broken
    }

    /// Whether a pass ended at its bound: the next round goes on with it,
    /// and is due at once -- a driver that kicks only when asked
    /// (EVENT_IDX) gives the device no other reason to come back.
    pub fn more(&self) -> bool {
        self.more
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
        c[at..at + 4].copy_from_slice(&self.seg_max().to_le_bytes());
        /* A sector, to a driver that reads it unoffered; and the backend's
         * block as the physical one, and as the least a request should be */
        let at = usize::from(CFG_BLK_SIZE);
        c[at..at + 4].copy_from_slice(&(SECTOR as u32).to_le_bytes());
        let per_block = self.block() / SECTOR;
        if per_block > 1 {
            c[usize::from(CFG_PHYSICAL_BLOCK_EXP)] = per_block.trailing_zeros() as u8;
            let at = usize::from(CFG_MIN_IO_SIZE);
            c[at..at + 2].copy_from_slice(&(per_block as u16).to_le_bytes());
        }
        for (at, value) in [(CFG_MAX_DISCARD_SECTORS, MAX_RANGE_SECTORS), (CFG_MAX_DISCARD_SEG, MAX_RANGES as u32),
                            (CFG_DISCARD_ALIGNMENT, self.backend.discard_sectors().clamp(1, MAX_RANGE_SECTORS)),
                            (CFG_MAX_ZEROES_SECTORS, MAX_RANGE_SECTORS),
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
                self.stats.notifies += 1;
                self.moved = 0;
                let answered = self.fill(mem);
                if answered { self.interrupt(mem) } else { Raise::default() }
            }
            Asked::Reset => {
                self.forget_chains();
                self.waiting = false;
                self.kickless = false;
                self.more = false;
                self.broken = None;
                Raise::default()
            }
            /* Its one queue set up anew, or taken down: the chains with the
             * backend were on the old rings. */
            Asked::Queue(0) => {
                self.forget_chains();
                Raise::default()
            }
            _ => Raise::default(),
        }
    }

    /// The chains with the backend made the old driver's -- or the old
    /// rings': nothing is given back for them, their buffers are taken back
    /// as they come, and nothing new is started until they have come
    /// (`fill`).
    fn forget_chains(&mut self) {
        for s in self.slots.iter_mut() {
            s.head = None;
        }
        /* A request parked was never handed over: its slot is free again */
        self.parked = None;
    }

    /// What the backend has served, given back to the guest -- a read's data
    /// into its buffers, the status, the chain on the used ring -- and what
    /// waited on the ring for a buffer, taken now there is one, or what a
    /// pass left at its bound -- and the interrupts that leaves the device
    /// with. A pass's worth of data at most (`PASS_BYTES`), the rest the
    /// next round's. Every time round the run loop, so it costs next to
    /// nothing when there is nothing.
    pub fn poll(&mut self, mem: &GuestMemory) -> Raise {
        self.moved = 0;
        let mut answered = false;
        let mut back = false;
        let mut cut = false;
        loop {
            if self.moved >= PASS_BYTES {
                /* Perhaps nothing more with the backend: a round for nothing
                 * then, which ends the bound. */
                cut = true;
                break;
            }
            let Some(req) = self.backend.take() else { break };
            back = true;
            answered |= self.complete(mem, req);
        }
        if cut {
            self.stats.cut += 1;
            self.more = true;
        } else if self.more || (back && (self.waiting || self.kickless)) {
            answered |= self.fill(mem);
        }
        if answered { self.interrupt(mem) } else { Raise::default() }
    }

    /// The interrupt, when the driver wants one: the line, when none is
    /// waiting already, or the queue's MSI-X vector.
    fn interrupt(&mut self, mem: &GuestMemory) -> Raise {
        let wants = self.transport.live(0).map_or(false, |q| q.wants_interrupt(mem));
        if !wants {
            return Raise::default();
        }
        self.stats.interrupts += 1;
        self.transport.interrupt(0)
    }

    /// Take what the driver has made available, while there is a buffer for
    /// it and a pass's worth at most: each request handed to the backend, or
    /// -- when there is nothing for the backend to do -- answered at once.
    /// True when something was given back.
    fn fill(&mut self, mem: &GuestMemory) -> bool {
        self.waiting = false;
        self.kickless = false;
        self.more = false;
        if self.slots.iter().any(Slot::orphan) {
            /* The old driver's requests, which a reset or the queue set up
             * anew left with the backend, done first: the ring is looked
             * at again as the last of them comes back (`poll`). */
            self.waiting = true;
            return false;
        }
        let mut answered = false;
        if let Some(p) = self.parked {
            if !self.may_start(p.op, p.offset, p.len) {
                /* Still waiting: looked at again as the next comes back */
                self.waiting = true;
                return false;
            }
            self.parked = None;
            let segs = core::mem::take(&mut self.slots[p.slot].segs);
            let result = self.serve(mem, p.slot, p.head, &segs, p.op, p.offset, p.len, p.ranges);
            self.slots[p.slot].segs = segs;
            match result {
                Ok(now) => answered |= now,
                Err(e) => {
                    self.broken = Some(e);
                    return answered;
                }
            }
        }
        if self.quiesced {
            /* Drained: nothing more off the ring until it goes on, which
             * looks again (`quiesce`). */
            return answered;
        }
        let mut left = PASS;
        while self.broken.is_none() {
            if left == 0 || self.moved >= PASS_BYTES {
                if left != 0 {
                    self.stats.cut += 1;
                }
                self.more = true;
                break;
            }
            left -= 1;
            let Some(slot) = self.slots.iter().position(|s| s.head.is_none() && !s.out) else {
                /* Every buffer is with the backend: the rest waits on the
                 * ring until one comes back. */
                self.waiting = true;
                break;
            };
            let mut segs = core::mem::take(&mut self.slots[slot].segs);
            let Some(q) = self.transport.live(0) else {
                /* No queue set up: nothing to take, and nothing to ask of
                 * its rings, which would be at page 0. */
                self.slots[slot].segs = segs;
                break;
            };
            let popped = q.pop(mem, &mut segs);
            let result = match popped {
                Ok(Some(head)) => self.start(mem, slot, head, &segs),
                Ok(None) => {
                    self.slots[slot].segs = segs;
                    /* Every chain there was taken. With KICKLESS_OUT or more
                     * requests out, one comes back soon, and the ring is
                     * looked at again then (`poll`): no kick asked for, so
                     * that a driver keeping its disk busy adds requests
                     * without leaving its guest for each -- the notify, an
                     * exit of 2.8 us of the host's on the EX44, was 41% of
                     * the job's CPU at QD32 (round 13). With fewer, a kick
                     * asked for the next, and what came before the ask
                     * taken now. */
                    if self.slots.iter().filter(|s| s.out).count() >= KICKLESS_OUT
                        && self.transport.live(0).is_some_and(|q| q.event_idx())
                    {
                        self.kickless = true;
                        break;
                    }
                    match self.transport.live(0).ok_or(Broken::Index).and_then(|q| q.ask_kick(mem)) {
                        Ok(true) => continue,
                        Ok(false) => break,
                        Err(e) => {
                            self.broken = Some(e);
                            break;
                        }
                    }
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
            if self.parked.is_some() {
                /* Nothing more taken until it is handed over: the ring is
                 * looked at again as a request comes back (`poll`). */
                break;
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
        if self.slots[slot].out {
            /* `fill` chose the slot for having its buffer. */
            self.stats.errors += 1;
            self.finish(mem, segs, head, S_IOERR, 0)?;
            return Ok(true);
        }
        /* The copy the ranges were checked in, not the guest's memory again */
        let mut copy = [0u8; MAX_RANGES * RANGE_BYTES];
        if let Some(ranges) = ranges {
            copy[..len].copy_from_slice(&ranges[..len]);
        }
        if !self.may_start(op, offset, len) {
            /* Held in its slot, as checked, until the writes it waits for
             * are back: nothing more is taken off the ring meanwhile, and
             * it is the first started then (`fill`). */
            self.slots[slot].head = Some(head);
            self.parked = Some(Parked { slot, head, op, offset, len, ranges: copy });
            self.waiting = true;
            return Ok(false);
        }
        self.serve(mem, slot, head, segs, op, offset, len, copy)
    }

    /// Hand the request at `head` over from `slot`, as checked: a write's
    /// data copied into its buffer first -- or, for part of a block, the
    /// blocks around it to be read first (`Pad`). False, or true where it
    /// was answered at once, failed.
    #[allow(clippy::too_many_arguments)]
    fn serve(&mut self, mem: &GuestMemory, slot: usize, head: u16, segs: &[Seg], op: Op, offset: u64, len: usize,
             ranges: [u8; MAX_RANGES * RANGE_BYTES]) -> core::result::Result<bool, Broken> {
        let previous = self.slots[slot].part;
        let first = previous.is_none();
        let done = previous.map_or(0, |p| p.done);
        let whole = previous.map_or(len, |p| p.left);
        let mut len = whole.min(REQUEST_BYTES);
        if matches!(op, Op::Read | Op::Write) {
            /* The blocks around a part must fit its buffer too. Cutting at
             * this bound leaves the next part on a backend block boundary. */
            let used = (offset % self.block()) as usize;
            len = len.min(REQUEST_BYTES - used);
            self.slots[slot].part = Some(Part { op, next: offset + len as u64, left: whole - len, done });
        }
        let (at, span) = self.span(op, offset, len);
        let pad = ((at, span) != (offset, len)).then_some(Pad { skip: (offset - at) as usize, len,
                                                                  reading: op == Op::Write });
        if op == Op::Write && pad.is_none() {
            self.moved = self.moved.saturating_add(len);
            if !self.backend.with_buffer(slot, &mut |buf| {
                buf.len() >= len && copy_out(mem, segs, HEADER as u64 + done as u64, &mut buf[..len])
            }) {
                self.stats.errors += 1;
                self.slots[slot].part = None;
                self.finish(mem, segs, head, S_IOERR, 0)?;
                return Ok(true);
            }
        }
        if pad.is_some_and(|p| p.reading) {
            self.rmw = true;
        }
        /* Part of a block: the blocks around it read, whichever it is */
        let (op, offset, len) = if pad.is_some() { (Op::Read, at, span) } else { (op, offset, len) };
        let s = &mut self.slots[slot];
        (s.head, s.out, s.wrote, s.pad) = (Some(head), true, op.writes(), pad);
        if first {
            s.at = kcore::time::cycle_counter();
        }
        /* Before the hand-off: what the backend's task does with it is
         * recorded after this. */
        crate::tp::blk_submit(self.index, slot, op, offset, len);
        self.backend.submit(Request { op, offset, len, ok: false, slot, ranges });
        Ok(false)
    }

    /// A request the backend has served, given back: its buffer to its slot
    /// whatever became of the chain, and the chain to the driver when there
    /// is still one to give it back to. True when something was.
    fn complete(&mut self, mem: &GuestMemory, req: Request) -> bool {
        let Request { op, offset, len, ok, slot, ranges } = req;
        crate::tp::blk_complete(self.index, slot, op, offset, len, ok);
        let flushes = self.transport.driver_features() & F_FLUSH != 0;
        let Some(s) = self.slots.get_mut(slot) else {
            return false;
        };
        let head = s.head.take();
        s.out = false;
        s.wrote = false;
        let at = s.at;
        let behind = s.behind.take();
        let pad = s.pad.take();
        let part = s.part;
        let (mut op, mut len, mut ok) = (op, len, ok);
        if let Some(p) = pad {
            if p.reading {
                /* A write's span, read: the guest's data merged in and the
                 * span written whole -- the write still the one write out */
                if let (Some(chain), true) = (head, ok && self.broken.is_none()) {
                    let segs = core::mem::take(&mut self.slots[slot].segs);
                    let merged = p.skip.checked_add(p.len).is_some_and(|end| end <= len)
                        && self.backend.with_buffer(slot, &mut |buf| {
                            buf.len() >= len && copy_out(mem, &segs, HEADER as u64 + part.map_or(0, |p| p.done) as u64,
                                                        &mut buf[p.skip..p.skip + p.len])
                        });
                    self.slots[slot].segs = segs;
                    if merged {
                        self.moved = self.moved.saturating_add(p.len);
                        let s = &mut self.slots[slot];
                        (s.head, s.out, s.wrote, s.pad) = (Some(chain), true, true, Some(Pad { reading: false, ..p }));
                        crate::tp::blk_submit(self.index, slot, Op::Write, offset, len);
                        self.backend.submit(Request { op: Op::Write, offset, len, ok: false, slot, ranges });
                        return false;
                    }
                }
                /* Not read, its data not had, or its chain gone: given up,
                 * answered -- where there is still a chain -- as a write
                 * that failed */
                self.rmw = false;
                ok = false;
            } else if op == Op::Write {
                /* Written back: other writes may be handed over again */
                self.rmw = false;
            }
            /* Answered as what the guest asked: its write, or its part of
             * the span read */
            op = if p.reading { Op::Write } else { op };
            len = p.len;
        }
        /* A read's part goes to its own place in the chain before the buffer
         * is reused. A later failure still answers the whole request failed. */
        let mut read = false;
        if let (Some(_), true) = (head, ok && op == Op::Read && self.broken.is_none()) {
            let skip = pad.map_or(0, |p| p.skip);
            let stream = part.map_or(0, |p| p.done) as u64;
            let segs = core::mem::take(&mut self.slots[slot].segs);
            read = skip.checked_add(len).is_some_and(|end| {
                self.backend.with_buffer(slot, &mut |buf| {
                    buf.len() >= end && copy_in(mem, &segs, stream, &buf[skip..skip + len])
                })
            });
            self.slots[slot].segs = segs;
            if read {
                self.moved = self.moved.saturating_add(len);
            } else {
                ok = false;
            }
        }
        let whole = part.map_or(len, |p| p.done + len);
        if let (Some(chain), Some(p), true) =
            (head, part, ok && self.broken.is_none() && part.is_some_and(|p| p.left != 0))
        {
            self.slots[slot].part = Some(Part { done: p.done + len, ..p });
            let segs = core::mem::take(&mut self.slots[slot].segs);
            let result = self.serve(mem, slot, chain, &segs, p.op, p.next, p.left, ranges);
            self.slots[slot].segs = segs;
            return match result {
                Ok(now) => now,
                Err(e) => {
                    self.slots[slot].part = None;
                    self.broken = Some(e);
                    false
                }
            };
        }
        if let Some(p) = part {
            op = p.op;
            len = whole;
        }
        let s = &mut self.slots[slot];
        if let (Some(chain), true, None) = (head, ok && op.writes() && !flushes, behind) {
            if self.broken.is_none() {
                /* Done, for a driver that took no FLUSH, which may take its
                 * disk to be write-through: flushed before it is answered,
                 * the flush in its slot and answering for it. */
                (s.head, s.out, s.behind) = (Some(chain), true, Some((op, len, ranges)));
                crate::tp::blk_submit(self.index, slot, Op::Flush, 0, 0);
                self.backend.submit(Request { op: Op::Flush, offset: 0, len: 0, ok: false, slot,
                                              ranges: [0; MAX_RANGES * RANGE_BYTES] });
                return false;
            }
        }
        /* A flush behind a write answers as the write it was behind. */
        let (op, len, ranges) = behind.unwrap_or((op, len, ranges));
        let segs = core::mem::take(&mut s.segs);
        let answered = match head {
            Some(head) if self.broken.is_none() => {
                let (status, written) = if !ok {
                    (S_IOERR, 0)
                } else if op != Op::Read {
                    (S_OK, 0)
                } else if read {
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
                            self.stats.discarded_bytes += ranged_bytes(&ranges[..len]);
                        }
                        Op::WriteZeroes => {
                            self.stats.zeroings += 1;
                            self.stats.zeroed_bytes += ranged_bytes(&ranges[..len]);
                        }
                    }
                } else {
                    self.stats.errors += 1;
                }
                /* A counter behind the hand-off -- another CPU's -- is no wait */
                let waited = crate::tp::ns(kcore::time::cycle_counter().saturating_sub(at));
                self.stats.wait_ns = self.stats.wait_ns.saturating_add(u64::from(waited));
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
        self.slots[slot].part = None;
        self.slots[slot].segs = segs;
        answered
    }

    /// Write the status -- the last byte the device may write -- and give
    /// the chain back with what was written.
    fn finish(&mut self, mem: &GuestMemory, segs: &[Seg], head: u16, status: u8, written: u32) -> core::result::Result<(), Broken> {
        /* The queue first: with it down nothing of the chain's is written
         * (a chain is never held past its queue going: `forget_chains`). */
        let q = self.transport.live(0).ok_or(Broken::Index)?;
        let last = segs.iter().rev().find(|s| s.write && s.len != 0).ok_or(Broken::Memory)?;
        let at = last.at(u64::from(last.len) - 1).ok_or(Broken::Memory)?;
        mem.write(at, &[status]).map_err(|_| Broken::Memory)?;
        q.push(mem, head, written.saturating_add(1))
    }

    /// What a request is, from its header: the backend's to serve, or
    /// answered here -- a request past the disk is an I/O error; one with no
    /// data is done. A read or write larger than a buffer is served in parts.
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
            /* Served whether or not the driver took FLUSH, as QEMU serves
             * it: what was answered as written is on stable storage after,
             * which no driver can be wrong to ask. */
            T_FLUSH => Next::Serve(Op::Flush, 0, 0),
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
        let bs = self.block();
        if bs == SECTOR {
            return Next::Ranges(op, count, ranges);
        }
        if op != Op::Discard {
            /* A write of zeros is not offered where a block is more than a
             * sector: a driver that was not told it may does not send one */
            return Next::Answer(S_UNSUPP, 0);
        }
        /* The whole blocks each range has, alone, given back -- a discard
         * need give back nothing -- and a range of no whole block left out:
         * the backend is handed whole blocks, as every request */
        let mut kept = [0u8; MAX_RANGES * RANGE_BYTES];
        let mut n = 0;
        for r in ranges[..count * RANGE_BYTES].chunks_exact(RANGE_BYTES) {
            let sector = u64::from_le_bytes([r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7]]);
            let sectors = u32::from_le_bytes([r[8], r[9], r[10], r[11]]);
            /* Inside the disk, checked above */
            let start = sector.saturating_mul(SECTOR);
            let end = start.saturating_add(u64::from(sectors) * SECTOR);
            let (first, last) = (start.div_ceil(bs).saturating_mul(bs), end / bs * bs);
            if last > first {
                let out = &mut kept[n * RANGE_BYTES..(n + 1) * RANGE_BYTES];
                out[..8].copy_from_slice(&(first / SECTOR).to_le_bytes());
                out[8..12].copy_from_slice(&(((last - first) / SECTOR) as u32).to_le_bytes());
                n += 1;
            }
        }
        if n == 0 {
            return Next::Answer(S_OK, 0);
        }
        Next::Ranges(op, n, kept)
    }

    /// A read or a write of `len` bytes from `sector`: the backend gets it
    /// in buffer-sized parts, whole blocks of its around each one.
    fn data_request(&mut self, op: Op, sector: u64, len: u64) -> Next {
        match self.range(sector, len) {
            Some(_) if len == 0 => Next::Answer(S_OK, 0),
            Some(offset) => match usize::try_from(len) {
                Ok(len) => Next::Serve(op, offset, len),
                Err(_) => {
                    self.stats.errors += 1;
                    Next::Answer(S_IOERR, 0)
                }
            },
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
        (len % SECTOR == 0 && end <= self.disk_bytes()).then_some(offset)
    }
}

/// The backend's block in bytes (`Backend::block_size`), held to what the
/// device takes: a power of two of sectors up to a page -- `SECTOR` for
/// anything else, which a backend never says.
fn block_bytes(backend: &dyn Backend) -> u64 {
    let bs = u64::from(backend.block_size());
    if bs.is_power_of_two() && bs >= SECTOR && bs <= SIZE_MAX as u64 { bs } else { SECTOR }
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
