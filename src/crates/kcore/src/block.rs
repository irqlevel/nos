use alloc::vec::Vec;
use ffi::block;

use crate::error::{Error, Result};
use crate::events::SharedEvent;
use crate::lent::Buffers;

pub use crate::lent::{Lent, Started};

pub use ffi::block::BlockIo;

/// What a BlockIo asks for
pub const IO_READ: u8 = 0;
pub const IO_WRITE: u8 = 1;
pub const IO_FLUSH: u8 = 2;

/// What a driver's submit answers
pub const SUBMIT_OK: i32 = 0;
pub const SUBMIT_BUSY: i32 = 1;
pub const SUBMIT_INVALID: i32 = 2;
pub const SUBMIT_UNSUPPORTED: i32 = 3;

/// The most bytes of a claim's holder's name the kernel keeps: a name is cut
/// there.
pub const HOLDER_MAX: usize = 48;

/// The longest id a device has (`Disk::ident`): `disk:` and a model and
/// serial number of 75 bytes at most, or `part:` and a GUID's 36.
pub const IDENT_MAX: usize = 80;

/// The longest name a device has, as `disks` shows it.
pub const NAME_MAX: usize = 16;

/// What a device's partition table says it is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PartKind {
    /// A whole disk
    Disk,
    /// A GPT partition's type GUID, as its entry has it
    Gpt([u8; 16]),
    /// An MBR partition's type byte
    Mbr(u8),
}

impl PartKind {
    /// GPT's EFI System partition, and MBR's.
    const ESP_GPT: [u8; 16] = guid_bytes(0xC12A7328, 0xF81F, 0x11D2, [0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B]);
    const ESP_MBR: u8 = 0xEF;
    /// GPT's BIOS boot partition: where GRUB puts its core on a GPT disk a
    /// BIOS boots.
    const BIOS_BOOT_GPT: [u8; 16] = guid_bytes(0x21686148, 0x6449, 0x6E6F, [0x74, 0x4E, 0x65, 0x65, 0x64, 0x45, 0x46, 0x49]);

    /// What a machine's firmware boots from -- an EFI System partition, a
    /// BIOS boot partition -- named; None for anything else.
    pub fn boots_the_machine(self) -> Option<&'static str> {
        match self {
            PartKind::Gpt(g) if g == Self::ESP_GPT => Some("an EFI System partition"),
            PartKind::Mbr(Self::ESP_MBR) => Some("an EFI System partition"),
            PartKind::Gpt(g) if g == Self::BIOS_BOOT_GPT => Some("a BIOS boot partition"),
            _ => None,
        }
    }
}

/// A GUID's 16 bytes as GPT keeps them -- the first three fields
/// little-endian -- from the numbers it is written as.
const fn guid_bytes(a: u32, b: u16, c: u16, d: [u8; 8]) -> [u8; 16] {
    let (a, b, c) = (a.to_le_bytes(), b.to_le_bytes(), c.to_le_bytes());
    [a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]
}

/// A GUID as the text everyone writes it as -- `704db7dd-9067-4e69-...`,
/// lowercase -- from its 16 bytes as GPT keeps them.
pub fn guid_text(g: &[u8; 16]) -> [u8; 36] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let order = [3usize, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15];
    let mut out = [b'-'; 36];
    let mut at = 0;
    for (i, &b) in order.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            at += 1;
        }
        out[at] = HEX[(g[b] >> 4) as usize];
        out[at + 1] = HEX[(g[b] & 0xF) as usize];
        at += 2;
    }
    out
}

/// What looking a device up by its id found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Found {
    None,
    One(usize),
    /// Several devices say they are the one: copies of one disk, or of one
    /// partition table. None of them is taken for it.
    Many(u32),
}

/// A short text the kernel copied out -- a device's name, its id, who holds
/// it -- kept here, so that nothing points into the kernel's.
#[derive(Clone, Copy)]
pub struct Word<const N: usize> {
    len: usize,
    bytes: [u8; N],
}

impl<const N: usize> Word<N> {
    const EMPTY: Self = Word { len: 0, bytes: [0; N] };

    /// What `fill` writes into N bytes and says the length of: as much as
    /// fits of it.
    fn filled(fill: impl FnOnce(*mut u8, usize) -> usize) -> Self {
        let mut w = Self::EMPTY;
        w.len = fill(w.bytes.as_mut_ptr(), N).min(N);
        w
    }

    /// `b`, as much of it as fits.
    pub fn from_bytes(b: &[u8]) -> Self {
        let mut w = Self::EMPTY;
        w.len = b.len().min(N);
        w.bytes[..w.len].copy_from_slice(&b[..w.len]);
        w
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    /// Its text: "?" for bytes that are not UTF-8, which the kernel's never
    /// are.
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(self.as_bytes()).unwrap_or("?")
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<const N: usize> core::fmt::Display for Word<N> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<const N: usize> core::fmt::Debug for Word<N> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}", self.as_str())
    }
}

/// Who holds a claim, as a refusal names it.
pub type Holder = Word<HOLDER_MAX>;

/// Why an asynchronous I/O was not taken
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitError {
    /// No room right now; there will be after a completion.
    Busy,
    /// Out of range, misaligned, or more than the device takes at once.
    Invalid,
    /// The device has only the synchronous path.
    Unsupported,
}

/// A block device to read and write -- a disk, or a partition of one -- by
/// the name `disks` shows it under. **For a loadable module**: this is the
/// block layer across the C ABI, which is the only seam something linked on
/// its own shares with it. Code inside the kernel image uses `block::Disk`,
/// the same shape with no ABI in between.
/// Block devices stay as long as the
/// kernel does, so a Disk holds nothing and copies freely. Reads and writes
/// are synchronous, and may come from several tasks at once. The drivers
/// DMA straight to and from the buffer, so give them one they can: page-
/// aligned, physically contiguous, and no bigger than they take in one I/O
/// (a page for virtio-blk, two for NVMe) -- a DmaBuffer.
///
/// Writing needs a `claim` first: it keeps mounts, the disk log and other
/// writers off the device -- and off the disk it is a partition of, and its
/// partitions -- while the writes go around them.
#[derive(Clone, Copy)]
pub struct Disk {
    handle: usize,
}

impl Disk {
    pub fn open(name: &str) -> Option<Self> {
        let handle = unsafe { block::kernel_blockdev_find(name.as_ptr(), name.len()) };
        if handle == 0 { None } else { Some(Self { handle }) }
    }

    /// How many devices the kernel has; `at` takes 0 up to it. The table only
    /// grows, so an index once valid stays so.
    pub fn count() -> u32 {
        block::kernel_blockdev_count()
    }

    /// The index'th device, or None.
    pub fn at(index: u32) -> Option<Self> {
        match block::kernel_blockdev_at(index) {
            0 => None,
            handle => Some(Self { handle }),
        }
    }

    /// The device whose id (`ident`) is `id`, its letters' case aside --
    /// none, or several, none of which is taken for it.
    pub fn find_ident(id: &str) -> core::result::Result<Self, Found> {
        let mut count = 0u32;
        /* `id` is `id.len()` bytes, and `count` is a u32 of ours. */
        let handle = unsafe { block::kernel_blockdev_find_ident(id.as_ptr(), id.len(), &mut count) };
        match (handle, count) {
            (0, 0) => Err(Found::None),
            (0, n) => Err(Found::Many(n)),
            (handle, _) => Ok(Self { handle }),
        }
    }

    /// The handle the kernel knows it by: what tells two Disks apart.
    pub fn handle(&self) -> usize {
        self.handle
    }

    /// Its name, as `disks` shows it -- this boot's, which another boot may
    /// give another device.
    pub fn name(&self) -> Word<NAME_MAX> {
        /* `out` takes `cap` bytes: the Word's own. */
        Word::filled(|out, cap| unsafe { block::kernel_blockdev_name(self.handle, out, cap) })
    }

    /// What it is found by again at the next boot, whatever its name then:
    /// `part:<GUID>` or `disk:<model>_<serial>` -- None when nothing says.
    pub fn ident(&self) -> Option<Word<IDENT_MAX>> {
        let id = Word::filled(|out, cap| unsafe { block::kernel_blockdev_ident(self.handle, out, cap) });
        (!id.is_empty()).then_some(id)
    }

    /// What its partition table says it is.
    pub fn kind(&self) -> PartKind {
        let mut out = [0u8; 16];
        /* `out` takes 16 bytes. */
        match unsafe { block::kernel_blockdev_kind(self.handle, out.as_mut_ptr()) } {
            1 => PartKind::Gpt(out),
            2 => PartKind::Mbr(out[0]),
            _ => PartKind::Disk,
        }
    }

    /// The disk it is a partition of, and where on it it starts: None for a
    /// whole disk.
    pub fn parent(&self) -> Option<(Disk, u64)> {
        match block::kernel_blockdev_parent(self.handle) {
            0 => None,
            handle => Some((Disk { handle }, block::kernel_blockdev_start(self.handle))),
        }
    }

    /// Who holds a claim on it -- on the device itself, not on one it
    /// overlaps -- if anyone.
    pub fn holder(&self) -> Option<Holder> {
        let h = Word::filled(|out, cap| unsafe { block::kernel_blockdev_holder(self.handle, out, cap) });
        (!h.is_empty()).then_some(h)
    }

    /// Who holds it or anything overlapping it -- what a `claim` would be
    /// refused for -- if anyone; nothing is claimed by asking, so a look
    /// never gets in the way of a claim of somebody else's.
    pub fn in_the_way(&self) -> Option<Holder> {
        let h = Word::filled(|out, cap| unsafe { block::kernel_blockdev_in_the_way(self.handle, out, cap) });
        (!h.is_empty()).then_some(h)
    }

    /// Whether writing to one of the two can touch the other: one device, a
    /// disk and a partition of it, two partitions that overlap.
    pub fn overlaps(&self, other: &Disk) -> bool {
        block::kernel_blockdev_overlaps(self.handle, other.handle) != 0
    }

    /// Whether it gives sectors back (`discard`).
    pub fn can_discard(&self) -> bool {
        block::kernel_blockdev_can_discard(self.handle) != 0
    }

    /// Its `count` sectors from `sector` given back to it: a writer's call,
    /// under its claim. IoError when the device failed or takes no such thing.
    pub fn discard(&self, sector: u64, count: u64) -> Result<()> {
        if block::kernel_blockdev_discard(self.handle, sector, count) == 0 { Ok(()) } else { Err(Error::IoError) }
    }

    /// Its size, in sectors
    pub fn sectors(&self) -> u64 {
        block::kernel_blockdev_capacity(self.handle)
    }

    pub fn sector_size(&self) -> u64 {
        block::kernel_blockdev_sector_size(self.handle)
    }

    /// Fills buf -- a whole number of sectors -- from `sector` on.
    pub fn read(&self, sector: u64, buf: &mut [u8]) -> Result<()> {
        let count = self.sector_count(buf.len())?;
        let rc = unsafe { block::kernel_blockdev_read(self.handle, sector, buf.as_mut_ptr(), count) };
        if rc == 0 { Ok(()) } else { Err(Error::IoError) }
    }

    /// Writes buf -- a whole number of sectors -- from `sector` on; with
    /// `fua`, past the device's write cache.
    pub fn write(&self, sector: u64, buf: &[u8], fua: bool) -> Result<()> {
        let count = self.sector_count(buf.len())?;
        let rc = unsafe {
            block::kernel_blockdev_write(self.handle, sector, buf.as_ptr(), count, fua as i32)
        };
        if rc == 0 { Ok(()) } else { Err(Error::IoError) }
    }

    pub fn flush(&self) -> Result<()> {
        let rc = block::kernel_blockdev_flush(self.handle);
        if rc == 0 { Ok(()) } else { Err(Error::IoError) }
    }

    /// The device, for writing to it around whatever else would: refused --
    /// with who holds it -- while a mounted filesystem, the disk log or
    /// another writer is on it, on the disk it is a partition of, or on a
    /// partition of it. While the DiskClaim lives none of those can have it.
    pub fn claim(&self) -> core::result::Result<DiskClaim, Holder> {
        self.claim_as("")
    }

    /// `claim`, held by `name` -- what a refusal of somebody else's tells
    /// them; cut at HOLDER_MAX bytes.
    pub fn claim_as(&self, name: &str) -> core::result::Result<DiskClaim, Holder> {
        let mut refused = Holder::EMPTY;
        let mut len = 0usize;
        /* `name` is `name.len()` bytes; `refused` takes HOLDER_MAX, and
         * `len` is ours. */
        let claim = unsafe {
            block::kernel_blockdev_claim(self.handle, name.as_ptr(), name.len(), refused.bytes.as_mut_ptr(),
                                         HOLDER_MAX, &mut len)
        };
        if claim != 0 {
            return Ok(DiskClaim { claim });
        }
        refused.len = len.min(HOLDER_MAX);
        if refused.len == 0 {
            refused = Word::from_bytes(b"something");
        }
        Err(refused)
    }

    /// How many partitions of it the kernel found
    pub fn partitions(&self) -> u32 {
        block::kernel_blockdev_partitions(self.handle)
    }

    /// Whether it takes asynchronous I/O -- `submit`: NVMe does, and so does
    /// a partition of an NVMe disk.
    pub fn can_submit(&self) -> bool {
        block::kernel_blockdev_can_submit(self.handle) != 0
    }

    /// Hands the device an I/O straight to or from physical memory and
    /// returns at once; io.done is called when the device is done, exactly
    /// once, from interrupt context. Task or softirq context. With kick false
    /// the device may leave its doorbell for `kick` -- one doorbell for a
    /// batch. The io is read before this returns, and may be submitted again
    /// as it is after Busy.
    ///
    /// # Safety
    /// io.phys names io.count sectors of physically contiguous memory, dword
    /// aligned, that stay valid -- and, for a read, untouched -- until io.done
    /// has run; io.ctx stays valid as long. io.done runs in interrupt context:
    /// no sleeping, allocating or freeing in it.
    #[inline]
    pub unsafe fn submit(&self, io: &BlockIo, kick: bool) -> core::result::Result<(), SubmitError> {
        match unsafe { block::kernel_blockdev_submit(self.handle, io, kick as i32) } {
            SUBMIT_OK => Ok(()),
            SUBMIT_BUSY => Err(SubmitError::Busy),
            SUBMIT_UNSUPPORTED => Err(SubmitError::Unsupported),
            _ => Err(SubmitError::Invalid),
        }
    }

    /// Rings the doorbell for what `submit(.., false)` queued.
    #[inline]
    pub fn kick(&self) {
        block::kernel_blockdev_kick(self.handle)
    }

    fn sector_count(&self, len: usize) -> Result<u32> {
        let size = self.sector_size() as usize;
        if size == 0 || len == 0 || len % size != 0 {
            return Err(Error::InvalidValue);
        }
        u32::try_from(len / size).map_err(|_| Error::InvalidValue)
    }
}

/// A claim on a block device, from Disk::claim; given back on drop.
pub struct DiskClaim {
    claim: usize,
}

impl Drop for DiskClaim {
    fn drop(&mut self) {
        block::kernel_blockdev_release(self.claim);
    }
}

/// A device held with a queue of its own (`block/src/devq.rs`): what a
/// guest's disk on a partition or a whole disk of nos's is served by, as
/// `fs::ImageQueue` serves one in a file. Buffers of the kernel's, one a
/// request -- a write's data put in before it is started, a read's taken
/// out once it is reaped -- and requests with the device as many at once as
/// are started, each given back whole as it is done. The device is claimed
/// for as long as the queue is open, for reading or for writing: no mount,
/// disk log or other writer has it, nor anything overlapping it. Shared as
/// `ImageQueue` is: whoever fills and empties the buffers and whoever starts
/// and reaps their requests; every call but `lend` and `wake` from one task.
pub struct DiskQueue {
    /* Its buffers before the queue that keeps them: dropped first. */
    buffers: Buffers,
    disk: Disk,
    q: usize,
}

/// Why a device's queue was not opened.
#[derive(Clone, Copy, Debug)]
pub enum QueueError {
    /// Who holds the device, or something overlapping it.
    Held(Holder),
    /// No memory, every queue taken, an event that is no event's, or
    /// arguments past what a queue takes.
    Refused,
}

impl core::fmt::Display for QueueError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            QueueError::Held(by) => write!(f, "in use by {}", by),
            QueueError::Refused => f.write_str("no memory for its queue"),
        }
    }
}

impl Disk {
    /// The device held with a queue -- `buffers` buffers of `bytes` each, a
    /// whole number of pages -- for writing with `write`, claimed for the
    /// holder `holder` names, which a refusal of anybody else's tells them.
    pub fn into_queue(self, write: bool, holder: &str, buffers: usize, bytes: usize)
        -> core::result::Result<DiskQueue, QueueError>
    {
        self.queue_on(0, write, holder, buffers, bytes)
    }

    /// `into_queue`, made on `event` -- the event of a task that waits for
    /// this and more: the queue's completions and wakes signal that event as
    /// well as the queue's own, which `wait` and `poll` still sleep on. The
    /// kernel holds the event until the queue is closed, so `event` may be
    /// dropped first.
    pub fn into_queue_on(self, event: &SharedEvent, write: bool, holder: &str, buffers: usize, bytes: usize)
        -> core::result::Result<DiskQueue, QueueError>
    {
        self.queue_on(event.handle(), write, holder, buffers, bytes)
    }

    fn queue_on(self, event: u64, write: bool, holder: &str, buffers: usize, bytes: usize)
        -> core::result::Result<DiskQueue, QueueError>
    {
        let mut at = Vec::new();
        at.try_reserve_exact(buffers).map_err(|_| QueueError::Refused)?;
        at.resize(buffers, core::ptr::null_mut());
        let mut refused = Holder::EMPTY;
        let mut len = 0usize;
        /* `at` takes `buffers` pointers, `holder` is its bytes, `refused`
         * takes HOLDER_MAX and `len` is ours; the device and the event are
         * looked up. */
        let q = unsafe {
            block::kernel_blockq_open(self.handle, i32::from(write), buffers, bytes, holder.as_ptr(), holder.len(),
                                      at.as_mut_ptr(), refused.bytes.as_mut_ptr(), HOLDER_MAX, &mut len, event)
        };
        if q == 0 {
            if len == 0 {
                return Err(QueueError::Refused);
            }
            refused.len = len.min(HOLDER_MAX);
            return Err(QueueError::Held(refused));
        }
        /* The kernel's buffers, `bytes` each, kept until the queue is closed
         * -- by this one's drop, after these. */
        match unsafe { Buffers::new(at, bytes) } {
            Ok(buffers) => Ok(DiskQueue { buffers, disk: self, q }),
            Err(_) => {
                block::kernel_blockq_close(q);
                Err(QueueError::Refused)
            }
        }
    }
}

impl DiskQueue {
    pub fn disk(&self) -> &Disk {
        &self.disk
    }

    /// The device's size, in bytes.
    pub fn size(&self) -> u64 {
        self.disk.sectors().saturating_mul(self.disk.sector_size())
    }

    pub fn buffers(&self) -> usize {
        self.buffers.count()
    }

    /// A buffer's bytes.
    pub fn buffer_len(&self) -> usize {
        self.buffers.len()
    }

    /// Buffer `k`'s bytes, while nobody else has them: None while they are
    /// lent already, or the device's.
    pub fn lend(&self, k: usize) -> Option<Lent<'_>> {
        self.buffers.lend(k)
    }

    /// Buffer `k`'s request -- `len` bytes from byte `offset` of the device
    /// read into it, or with `write` written from it, from its start --
    /// started: `InFlight` until `reap` gives it back, or `Done`. IoError for
    /// one that failed, Busy for a buffer lent or started already. Its
    /// doorbell waits for `kick`.
    pub fn start(&self, k: usize, write: bool, offset: u64, len: usize) -> Result<Started> {
        self.buffers.take(k)?;
        match block::kernel_blockq_start(self.q, k, i32::from(write), offset, len) {
            1 => Ok(Started::InFlight),
            rc => {
                self.buffers.untake(k);
                if rc == 0 { Ok(Started::Done) } else { Err(Error::IoError) }
            }
        }
    }

    /// The doorbell for the requests started.
    pub fn kick(&self) {
        block::kernel_blockq_kick(self.q);
    }

    /// Each request done, as `done` is told -- its buffer, nobody's again,
    /// and whether it went well -- until there are no more: how many.
    pub fn reap(&self, mut done: impl FnMut(usize, bool)) -> usize {
        const BATCH: usize = 32;
        let mut total = 0;
        loop {
            let mut out = [block::QueueDone::default(); BATCH];
            /* `out` takes BATCH of them, and the queue is looked up. */
            let n = unsafe { block::kernel_blockq_reap(self.q, out.as_mut_ptr(), BATCH) };
            let Ok(n) = usize::try_from(n) else { return total };
            for d in &out[..n.min(BATCH)] {
                let k = d.buffer as usize;
                /* Only what was started comes back: anything else is a
                 * broken promise of the kernel's, which no request of the
                 * caller's may be taken for. */
                assert!(self.buffers.give_back(k), "a device's queue gave back buffer {}, which it did not have", k);
                done(k, d.ok != 0);
            }
            total += n;
            if n < BATCH {
                return total;
            }
        }
    }

    /// Sleeps until one of the device's commands is done, or a `wake`, or
    /// `timeout`: whether one came.
    pub fn wait(&self, timeout: crate::time::Duration) -> bool {
        block::kernel_blockq_wait(self.q, timeout.as_nanos()) == 1
    }

    /// What `wait` sleeps for, looked for for up to `poll` with this CPU
    /// spinning: whether it came.
    pub fn poll(&self, poll: crate::time::Duration) -> bool {
        block::kernel_blockq_poll(self.q, poll.as_nanos()) == 1
    }

    /// Wakes whoever waits (`wait`), from anywhere.
    pub fn wake(&self) {
        block::kernel_blockq_wake(self.q);
    }

    /// The device's write cache pushed out.
    pub fn flush(&self) -> Result<()> {
        if block::kernel_blockq_flush(self.q) == 0 { Ok(()) } else { Err(Error::IoError) }
    }

    /// `len` bytes from byte `offset` -- whole sectors, inside the device --
    /// given back to it, for a queue opened for writing.
    pub fn discard(&self, offset: u64, len: u64) -> Result<()> {
        if block::kernel_blockq_discard(self.q, offset, len) == 0 { Ok(()) } else { Err(Error::IoError) }
    }
}

impl Drop for DiskQueue {
    fn drop(&mut self) {
        /* Waits for what the device still has, frees the buffers and gives
         * the claim back: nothing reaches the buffers after -- a `Lent`
         * borrows the queue, and so is gone before this runs. */
        block::kernel_blockq_close(self.q);
    }
}
