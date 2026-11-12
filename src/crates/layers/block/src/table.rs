//! The kernel's block device table: every disk and partition the kernel has,
//! what `disks` lists, and the claims that keep two writers off the same
//! sectors.
//!
//! A driver registers a `BlockDriver` with it; a partition is an entry of the
//! table itself. The rest of the kernel image reaches a device through
//! `Disk` (disk.rs), which calls the functions here directly. What is left of
//! a C ABI -- the `kernel_blockdev_*` names at the bottom -- is what a
//! loadable module binds by name (`kcore::block::Disk`), because a module is
//! linked on its own, and one call the C++ boot path makes.
//!
//! A device is a handle, and a handle is a slot in the table plus one, so
//! zero is never a device. The table only grows: nothing unregisters, which
//! is what makes a lookup lock-free -- a slot is filled once and only read
//! after. The claims are the one part that needs a lock, and sit inside the
//! kernel's spinlock.

use kcore::owner::Owner;
use alloc::boxed::Box;
use alloc::ffi::CString;
use core::ffi::CStr;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use ffi::block::BlockIo;
use kcore::block::{SubmitError, IO_FLUSH, IO_READ, SUBMIT_BUSY, SUBMIT_INVALID, SUBMIT_OK, SUBMIT_UNSUPPORTED};
use kcore::cmd::Output;
use kcore::consts::PAGE_SIZE;
use kcore::dma::DmaBuffer;
use kcore::once::{Once, OnceBox};
use kcore::sync::SpinLock;
use kcore::klog;

/// What the table holds.
pub const MAX_DEVICES: usize = 48;

/// One piece of a batch (`Disk::write_pieces`, `Disk::read_pieces`): `len`
/// bytes of the batch's buffer from `at`, to or from the device from
/// `sector` on. A piece is a page of the buffer or the start of one -- `at`
/// on a page boundary, `len` whole sectors and at most a page -- which is
/// what every driver's own one-I/O path takes: a driver that cannot do
/// better does a batch one piece after another.
#[derive(Clone, Copy, Debug)]
pub struct Piece {
    pub sector: u64,
    pub at: usize,
    pub len: usize,
}

/// A block device, as the driver behind it: what the table calls when
/// somebody reads, writes or flushes the disk.
///
/// The driver is something that lives for good -- a device is registered for
/// the life of the kernel, and the table has no way to give one back -- and
/// every call can arrive from any task on any CPU, several at once: hence
/// `Sync`, and `&'static self`. What a call needs exclusively the driver
/// keeps behind a lock of its own.
pub trait BlockDriver: Sync + 'static {
    /// Its size, in sectors.
    fn capacity(&self) -> u64;

    /// Bytes to a sector.
    fn sector_size(&self) -> u64;

    /// Fill `buf` -- whole sectors, never empty: a request for none is
    /// answered before it gets here -- from `sector` on, and return once the
    /// data is in it. The device may be pointed straight at the buffer: the
    /// caller has given one it can DMA into.
    fn read(&'static self, sector: u64, buf: &mut [u8]) -> bool;

    /// Write `data` -- whole sectors, never empty -- at `sector`, and return
    /// once the device has it; with `fua`, once it is on the medium.
    fn write(&'static self, sector: u64, data: &[u8], fua: bool) -> bool;

    /// Push the device's write cache out. A device without one has nothing
    /// to do, which is the default.
    fn flush(&'static self) -> bool {
        true
    }

    /// Write each of `pieces` of `buf` at its sector, `base` sectors on --
    /// where the partition the table was asked for starts -- and return
    /// once every one is on the device. False when one failed; and by then
    /// each piece has been written or given up on, so no command of the
    /// batch has `buf` any more. The table has checked every piece against
    /// the buffer and the device (`Piece`). The default writes them one
    /// after another; a device that can have several in flight at once has
    /// them so.
    fn write_pieces(&'static self, base: u64, buf: &DmaBuffer, pieces: &[Piece]) -> bool {
        pieces.iter().all(|p| self.write(base.saturating_add(p.sector), &buf.as_slice()[p.at..p.at + p.len], false))
    }

    /// Fill each of `pieces` of `buf` from its sector, `base` sectors on,
    /// and return once every one is in it -- on the same terms as
    /// `write_pieces`.
    fn read_pieces(&'static self, base: u64, buf: &mut DmaBuffer, pieces: &[Piece]) -> bool {
        for p in pieces {
            if !self.read(base.saturating_add(p.sector), &mut buf.as_mut_slice()[p.at..p.at + p.len]) {
                return false;
            }
        }
        true
    }

    /// Whether the device has the asynchronous path -- `submit` and `kick`.
    /// Without it the table answers Unsupported for the driver.
    fn is_async(&self) -> bool {
        false
    }

    /// One asynchronous I/O straight to or from physical memory: never
    /// blocks, never waits. `io.done` is called exactly once when the device
    /// is done, from interrupt context. With `kick` false the doorbell may
    /// be left for `kick`. That `io.phys` is memory the device may use is
    /// what whoever called `Disk::submit` promised; the driver passes it on.
    fn submit(&'static self, _io: &BlockIo, _kick: bool) -> Result<(), SubmitError> {
        Err(SubmitError::Unsupported)
    }

    /// Ring the doorbell for what `submit` queued without one.
    fn kick(&'static self) {}

    /// The most bytes one asynchronous I/O moves from a page's start: two
    /// pages, or what the device takes through lists of them. 0 where there
    /// is no asynchronous path, which is the default.
    fn submit_max(&self) -> usize {
        0
    }

    /// What the disk is known by from one boot to the next, whatever its
    /// name is this time: what it says it is and its serial number, as one
    /// word of `[A-Za-z0-9._-]` -- `nvme-<model>_<serial>`. None where it
    /// says nothing of the kind, which is the default: such a disk is not
    /// found again by id (`find_ident`).
    fn identity(&self) -> Option<&str> {
        None
    }

    /// Whether the device takes `discard`.
    fn can_discard(&self) -> bool {
        false
    }

    /// The `count` sectors from `sector` -- never none, and inside the
    /// device: the table checked -- no longer needed, for the device to give
    /// back, and return once it has been told. False when it failed, or the
    /// device takes no such thing, which is the default. What a later read
    /// of them gives is the device's to say.
    fn discard(&'static self, _sector: u64, _count: u64) -> bool {
        false
    }
}

/// The longest id a device has (`Device::ident`), what a partition table
/// says a partition is, a lookup's answer and a GUID's text: kcore's, which
/// a module reaches a device through too.
pub use kcore::block::{guid_text, Found, PartKind, IDENT_MAX};

/// What a partition's table says it is: its type, and what it is known by
/// from boot to boot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PartId {
    /// A GPT entry: its unique GUID and its type GUID, as the entry has
    /// them (the first three fields little-endian).
    Gpt { unique: [u8; 16], kind: [u8; 16] },
    /// An MBR entry: the disk's signature, its number (1 to 4, 5 on for
    /// the logical ones) and its type byte. A signature of 0 says nothing.
    Mbr { signature: u32, number: u32, kind: u8 },
}

/// The id a partition is found by again (`find_ident`), from what its
/// table says: `part:<its unique GUID>`, or `part:<signature>-<number>` as
/// Linux writes an MBR one -- None for an MBR with no signature, which
/// nothing tells from another such disk's.
fn part_ident(id: &PartId) -> Option<Box<str>> {
    let mut text = alloc::string::String::new();
    text.try_reserve_exact(IDENT_MAX).ok()?;
    match id {
        PartId::Gpt { unique, .. } => {
            if unique.iter().all(|&b| b == 0) {
                return None;
            }
            text.push_str("part:");
            text.push_str(core::str::from_utf8(&guid_text(unique)).ok()?);
        }
        PartId::Mbr { signature, number, .. } => {
            if *signature == 0 {
                return None;
            }
            write!(text, "part:{:08x}-{:02x}", signature, number).ok()?;
        }
    }
    Some(text.into_boxed_str())
}

/// The id a whole disk is found by again: `disk:` and what its driver says
/// (`BlockDriver::identity`), when that is a word of `[A-Za-z0-9._-]` short
/// enough -- None otherwise, the disk then found by its name alone.
fn disk_ident(word: Option<&str>) -> Option<Box<str>> {
    let word = word?;
    if word.is_empty() || word.len() + 5 > IDENT_MAX
        || !word.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return None;
    }
    let mut text = alloc::string::String::new();
    text.try_reserve_exact(word.len() + 5).ok()?;
    text.push_str("disk:");
    text.push_str(word);
    Some(text.into_boxed_str())
}

/// A registered device, kept for the life of the kernel because nothing
/// takes a device back.
struct Device {
    /// With its NUL, which is how `kernel_blockdev_*` hands a name out
    name: Box<CStr>,
    capacity: u64,
    sector_size: u64,
    /// The disk a partition is on, as its handle; 0 for a whole disk
    parent: usize,
    backend: Backend,
    /// What it is found by again at the next boot, whatever its name is
    /// then (`find_ident`): None when nothing says.
    ident: Option<Box<str>>,
    /// What its partition table says it is.
    kind: PartKind,
}

/// What does a device's I/O.
enum Backend {
    /// A driver
    Driver(&'static dyn BlockDriver),
    /// A stretch of `parent`, from this sector of it: everything asked of a
    /// partition is rebased onto its disk, and refused past its own end.
    Partition { start: u64 },
}

impl Device {
    fn name(&self) -> &[u8] {
        self.name.to_bytes()
    }

    /// Whether [sector, sector + count) is inside the device. By subtraction
    /// from the size rather than by adding to the offset, so nothing can
    /// wrap.
    fn within(&self, sector: u64, count: u64) -> bool {
        sector <= self.capacity && count <= self.capacity - sector
    }

    /// How many sectors `bytes` is: None unless it is a whole number of them.
    fn sectors_in(&self, bytes: usize) -> Option<u64> {
        let size = usize::try_from(self.sector_size).ok()?;
        if size == 0 || bytes % size != 0 {
            return None;
        }
        Some((bytes / size) as u64)
    }
}

/// A slot for `dev`, and its handle; 0 when the table is full.
fn add(dev: Device) -> usize {
    let slot = match reserve() {
        Some(slot) => slot,
        None => {
            klog!(0, "block: the device table is full at {}", MAX_DEVICES);
            return 0;
        }
    };

    if let Some(dev) = DEVICES[slot].get_or_try_init(|| Some(Box::new(dev))) {
        klog!(0, "block: {} registered, {} sectors of {} bytes",
            core::str::from_utf8(dev.name()).unwrap_or("?"), dev.capacity, dev.sector_size);
    }
    slot + 1
}

/// Register `driver` as the block device `name`: its handle, or 0 when the
/// table is full or the name will not do. For good -- nothing unregisters.
pub fn register_driver(name: &str, parent: usize, driver: &'static dyn BlockDriver) -> usize {
    let name = match CString::new(name) {
        Ok(name) if !name.as_bytes().is_empty() => name.into_boxed_c_str(),
        _ => return 0,
    };

    add(Device {
        name,
        capacity: driver.capacity(),
        sector_size: driver.sector_size(),
        parent,
        backend: Backend::Driver(driver),
        ident: disk_ident(driver.identity()),
        kind: PartKind::Disk,
    })
}

/// One partition of the device `parent` names, as a device of its own, as
/// its table entry `id` says it is. The asynchronous path is there exactly
/// when the disk has one -- a caller picks its path by `can_submit`, and a
/// partition that claimed one its disk cannot serve would have every
/// submission refused.
pub fn register_partition(parent: usize, start: u64, count: u64, name: &CStr, id: &PartId) -> bool {
    let sector_size = match device(parent) {
        Some(disk) if disk.within(start, count) && count != 0 => disk.sector_size,
        _ => return false,
    };
    let kind = match *id {
        PartId::Gpt { kind, .. } => PartKind::Gpt(kind),
        PartId::Mbr { kind, .. } => PartKind::Mbr(kind),
    };

    add(Device {
        name: name.into(),
        capacity: count,
        sector_size,
        parent,
        backend: Backend::Partition { start },
        ident: part_ident(id),
        kind,
    }) != 0
}

/* ---- I/O, by handle ----
 *
 * A buffer is whole sectors and handed on as it is: a driver DMAs to it, and
 * that it can is the contract of whoever called in. A partition is its disk
 * a few sectors on, and a disk was registered before any partition of it, so
 * the walk up ends. */

pub(crate) fn read(handle: usize, sector: u64, buf: &mut [u8]) -> bool {
    let dev = match device(handle) {
        Some(dev) => dev,
        None => return false,
    };
    let count = match dev.sectors_in(buf.len()) {
        Some(0) => return true,
        Some(count) => count,
        None => return false,
    };

    /* Inside the device, whatever the device is: a driver is never asked
     * for a sector it does not have, whoever worked the number out -- a
     * filesystem from what its superblock claims, a shell command. */
    if !dev.within(sector, count) {
        return false;
    }
    match dev.backend {
        Backend::Driver(driver) => driver.read(sector, buf),
        Backend::Partition { start } => read(dev.parent, start + sector, buf),
    }
}

pub(crate) fn write(handle: usize, sector: u64, data: &[u8], fua: bool) -> bool {
    let dev = match device(handle) {
        Some(dev) => dev,
        None => return false,
    };
    let count = match dev.sectors_in(data.len()) {
        Some(0) => return true,
        Some(count) => count,
        None => return false,
    };

    if !dev.within(sector, count) {
        return false;
    }
    match dev.backend {
        Backend::Driver(driver) => driver.write(sector, data, fua),
        Backend::Partition { start } => write(dev.parent, start + sector, data, fua),
    }
}

/// Where a batch goes on the device `handle` names: the driver, and the
/// sector the pieces are offset by there -- the partition's start on its
/// disk. None, and why traced, unless every piece is one (`Piece`): inside
/// the buffer of `buf_len` bytes, a page of it or the start of one, whole
/// sectors, and inside the device. The device's own bounds are enough: a
/// partition is inside its disk, which the table checked when it made it.
fn batch_target(handle: usize, buf_len: usize, pieces: &[Piece]) -> Option<(&'static dyn BlockDriver, u64)> {
    let top = device(handle)?;
    for p in pieces {
        let inside = match top.sectors_in(p.len) {
            Some(count) if count != 0 => p.at % PAGE_SIZE == 0 && p.len <= PAGE_SIZE
                && p.at.checked_add(p.len).is_some_and(|end| end <= buf_len)
                && top.within(p.sector, count),
            _ => false,
        };
        if !inside {
            klog!(0, "block: {}: a batch piece of {} bytes at {} of the buffer, to sector {}, is none a device takes",
                core::str::from_utf8(top.name()).unwrap_or("?"), p.len, p.at, p.sector);
            return None;
        }
    }

    /* Down to the disk. A disk was registered before any partition of it,
     * so the walk ends; the bound is the table's size all the same. */
    let mut dev = top;
    let mut base = 0u64;
    for _ in 0..MAX_DEVICES {
        match dev.backend {
            Backend::Driver(driver) => return Some((driver, base)),
            Backend::Partition { start } => {
                base = base.checked_add(start)?;
                dev = device(dev.parent)?;
            }
        }
    }
    None
}

pub(crate) fn write_pieces(handle: usize, buf: &DmaBuffer, pieces: &[Piece]) -> bool {
    if pieces.is_empty() {
        return true;
    }
    match batch_target(handle, buf.len(), pieces) {
        Some((driver, base)) => driver.write_pieces(base, buf, pieces),
        None => false,
    }
}

pub(crate) fn read_pieces(handle: usize, buf: &mut DmaBuffer, pieces: &[Piece]) -> bool {
    if pieces.is_empty() {
        return true;
    }
    match batch_target(handle, buf.len(), pieces) {
        Some((driver, base)) => driver.read_pieces(base, buf, pieces),
        None => false,
    }
}

pub(crate) fn flush(handle: usize) -> bool {
    match device(handle) {
        Some(dev) => match dev.backend {
            Backend::Driver(driver) => driver.flush(),
            Backend::Partition { .. } => flush(dev.parent),
        },
        None => false,
    }
}

/// Whether the device takes `discard`: its disk's driver does.
pub(crate) fn can_discard(handle: usize) -> bool {
    match device(handle) {
        Some(dev) => match dev.backend {
            Backend::Driver(driver) => driver.can_discard(),
            Backend::Partition { .. } => can_discard(dev.parent),
        },
        None => false,
    }
}

/// The `count` sectors from `sector` given back to the device -- inside it,
/// whoever worked the numbers out, as a read or a write is: whether the
/// device was told.
pub(crate) fn discard(handle: usize, sector: u64, count: u64) -> bool {
    let dev = match device(handle) {
        Some(dev) => dev,
        None => return false,
    };
    if count == 0 {
        return true;
    }
    if !dev.within(sector, count) {
        return false;
    }
    match dev.backend {
        Backend::Driver(driver) => driver.can_discard() && driver.discard(sector, count),
        Backend::Partition { start } => discard(dev.parent, start + sector, count),
    }
}

pub(crate) fn can_submit(handle: usize) -> bool {
    match device(handle) {
        Some(dev) => match dev.backend {
            Backend::Driver(driver) => driver.is_async(),
            Backend::Partition { .. } => can_submit(dev.parent),
        },
        None => false,
    }
}

pub(crate) fn submit_max(handle: usize) -> usize {
    match device(handle) {
        Some(dev) => match dev.backend {
            Backend::Driver(driver) if driver.is_async() => driver.submit_max(),
            Backend::Driver(_) => 0,
            Backend::Partition { .. } => submit_max(dev.parent),
        },
        None => 0,
    }
}

pub(crate) fn kick(handle: usize) {
    if let Some(dev) = device(handle) {
        match dev.backend {
            Backend::Driver(driver) if driver.is_async() => driver.kick(),
            Backend::Driver(_) => {}
            Backend::Partition { .. } => kick(dev.parent),
        }
    }
}

pub(crate) fn submit(handle: usize, io: &BlockIo, kick_now: bool) -> Result<(), SubmitError> {
    let dev = device(handle).ok_or(SubmitError::Invalid)?;

    /* A flush is about the device, not about a range of it: passed on as it
     * is. Anything else is inside the device, as a synchronous request is. */
    if io.op != IO_FLUSH && !dev.within(io.sector, io.count as u64) {
        /* Refused, but a kick is still a kick: what was queued before it
         * without a doorbell is owed one. */
        if kick_now {
            kick(handle);
        }
        return Err(SubmitError::Invalid);
    }

    let start = match dev.backend {
        Backend::Driver(driver) if driver.is_async() => return driver.submit(io, kick_now),
        Backend::Driver(_) => return Err(SubmitError::Unsupported),
        Backend::Partition { start } => start,
    };

    if io.op == IO_FLUSH {
        return submit(dev.parent, io, kick_now);
    }

    /* Moved onto the disk in a copy: the caller's io is only read, so it can
     * be submitted again as it is after a Busy. */
    let on_disk = BlockIo {
        op: io.op,
        fua: io.fua,
        reserved: io.reserved,
        count: io.count,
        sector: start + io.sector,
        phys: io.phys,
        done: io.done,
        ctx: io.ctx,
    };
    submit(dev.parent, &on_disk, kick_now)
}

static DEVICES: [OnceBox<Device>; MAX_DEVICES] = [const { OnceBox::new() }; MAX_DEVICES];
static COUNT: AtomicU32 = AtomicU32::new(0);

fn device(handle: usize) -> Option<&'static Device> {
    DEVICES.get(handle.checked_sub(1)?)?.get()
}

/// Take the next free slot, or None when the table is full.
fn reserve() -> Option<usize> {
    loop {
        let taken = COUNT.load(Ordering::Acquire);
        if taken as usize >= MAX_DEVICES {
            return None;
        }
        if COUNT
            .compare_exchange(taken, taken + 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return Some(taken as usize);
        }
    }
}

/* ---- the table, by handle ---- */

/// How many devices the table holds. It only grows, so an index once valid
/// stays valid and names the same device.
pub(crate) fn count() -> u32 {
    COUNT.load(Ordering::Acquire)
}

/// The index'th device, or 0. A slot being reserved by a registration that
/// has not finished reads as 0, and the caller skips it.
pub(crate) fn at(index: u32) -> usize {
    let index = index as usize;
    match DEVICES.get(index) {
        Some(slot) if slot.get().is_some() => index + 1,
        _ => 0,
    }
}

/// The device of that name, or 0.
pub(crate) fn find(wanted: &[u8]) -> usize {
    if wanted.is_empty() {
        return 0;
    }

    for index in 0..count() {
        let handle = at(index);
        match device(handle) {
            Some(dev) if dev.name() == wanted => return handle,
            _ => continue,
        }
    }
    0
}

/// Whether the handle names a device at all.
pub(crate) fn exists(handle: usize) -> bool {
    device(handle).is_some()
}

/// The device's name: its own copy, kept as long as the device, which is
/// for good.
pub(crate) fn name(handle: usize) -> Option<&'static str> {
    core::str::from_utf8(device(handle)?.name()).ok()
}

/// The disk a partition is on, or 0 for a whole disk.
pub(crate) fn parent(handle: usize) -> usize {
    device(handle).map_or(0, |dev| dev.parent)
}

pub(crate) fn capacity(handle: usize) -> u64 {
    device(handle).map_or(0, |dev| dev.capacity)
}

pub(crate) fn sector_size(handle: usize) -> u64 {
    device(handle).map_or(0, |dev| dev.sector_size)
}

/// The sector of its disk a partition starts at; None for a whole disk.
pub(crate) fn start(handle: usize) -> Option<u64> {
    match device(handle)?.backend {
        Backend::Partition { start } => Some(start),
        Backend::Driver(_) => None,
    }
}

/// What the device is found by again at the next boot (`find_ident`).
pub(crate) fn ident(handle: usize) -> Option<&'static str> {
    device(handle)?.ident.as_deref()
}

/// What the device's partition table says it is; `Disk` for a whole disk,
/// and for a handle that names nothing.
pub(crate) fn kind(handle: usize) -> PartKind {
    device(handle).map_or(PartKind::Disk, |dev| dev.kind)
}

/// The device whose id (`ident`) is `wanted`, its letters' case aside: a
/// GUID is written either way.
pub(crate) fn find_ident(wanted: &[u8]) -> Found {
    if wanted.is_empty() {
        return Found::None;
    }
    let mut found = Found::None;
    for index in 0..count() {
        let handle = at(index);
        let Some(id) = device(handle).and_then(|dev| dev.ident.as_deref()) else { continue };
        if id.as_bytes().eq_ignore_ascii_case(wanted) {
            found = match found {
                Found::None => Found::One(handle),
                Found::One(_) => Found::Many(2),
                Found::Many(n) => Found::Many(n.saturating_add(1)),
            };
        }
    }
    found
}

/// Whether writing to one of the two devices can touch the other (`overlap`).
pub(crate) fn overlaps(a: usize, b: usize) -> bool {
    overlap(a, b)
}

/// How many partitions of the device the kernel found.
pub(crate) fn partitions(handle: usize) -> u32 {
    if handle == 0 {
        return 0;
    }

    let mut found = 0;
    for index in 0..count() {
        let other = at(index);
        if other != 0 && parent(other) == handle {
            found += 1;
        }
    }
    found
}

/// Set once interrupts and the scheduler are running (the boot path calls
/// it). Before that a synchronous I/O has to poll its device: there is
/// nothing yet to wake a waiter.
static INTERRUPTS_STARTED: AtomicBool = AtomicBool::new(false);

/// Whether a driver may block waiting for a completion: false early in
/// boot, before interrupts and the scheduler are running, when a
/// synchronous I/O has to poll its device instead.
pub fn interrupts_started() -> bool {
    INTERRUPTS_STARTED.load(Ordering::Acquire)
}

/* ---- claims ---- */

/// A claim is the slot plus one in its low bits and a count of claims above
/// them, so a stale claim never releases the slot's next one.
const SLOT_BITS: u32 = 8;
const SLOT_MASK: usize = (1 << SLOT_BITS) - 1;
const _: () = assert!(MAX_DEVICES < (1 << SLOT_BITS), "a slot must fit a claim");

/// The most bytes of a holder's name a claim keeps: more is cut there.
pub use kcore::block::HOLDER_MAX;

/// Who holds a claim, as a refusal names it: the kernel's own name for the
/// holder, or the one a module gave -- copied either way, so that it lasts
/// as long as the claim whoever gave it, a module that may go first
/// included.
#[derive(Clone, Copy)]
pub struct Holder {
    len: u8,
    bytes: [u8; HOLDER_MAX],
}

const _: () = assert!(HOLDER_MAX <= u8::MAX as usize, "a holder's length is a byte");

impl Holder {
    const NONE: Holder = Holder { len: 0, bytes: [0; HOLDER_MAX] };

    /// `name`, cut at HOLDER_MAX bytes -- before a character the cut would
    /// split.
    pub fn named(name: &[u8]) -> Holder {
        let mut len = name.len().min(HOLDER_MAX);
        while len > 0 && len < name.len() && name[len] & 0xC0 == 0x80 {
            len -= 1;
        }
        let mut bytes = [0; HOLDER_MAX];
        bytes[..len].copy_from_slice(&name[..len]);
        Holder { len: len as u8, bytes }
    }

    pub fn of(name: &CStr) -> Holder {
        Holder::named(name.to_bytes())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }
}

impl core::fmt::Display for Holder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(core::str::from_utf8(self.as_bytes()).unwrap_or("something"))
    }
}

impl core::fmt::Debug for Holder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}", core::str::from_utf8(self.as_bytes()).unwrap_or("something"))
    }
}

#[derive(Clone, Copy)]
struct ClaimEntry {
    device: usize,
    /// Who holds it: handed back to whoever is refused because of this
    /// claim.
    holder: Holder,
    /// 0: the slot is free
    claim: usize,
    /// The module whose code took it (the image for the image's): what
    /// `rust_block_module_gone` releases by.
    owner: Owner,
}

const NO_CLAIM: ClaimEntry = ClaimEntry { device: 0, holder: Holder::NONE, claim: 0, owner: Owner::IMAGE };

struct Claims {
    entries: [ClaimEntry; MAX_DEVICES],
    generation: usize,
}

static CLAIMS: Once<SpinLock<Claims>> = Once::new();

const TOO_MANY: &CStr = c"too many claims already";
const NOT_READY: &CStr = c"the block layer, still starting up";
const NO_DEVICE: &CStr = c"nothing -- there is no such device";
pub(crate) const MODULE_HOLDER: &CStr = c"a module writing to it";

/// Called once, from `block::init`, before anything can claim: the table
/// needs the kernel's spinlock, which is a handle and cannot be a static.
pub fn claims_setup() -> bool {
    if CLAIMS.get().is_some() {
        return true;
    }

    match SpinLock::new(Claims { entries: [NO_CLAIM; MAX_DEVICES], generation: 0 }) {
        /* Lost to another setup: there is a table, which is what was asked. */
        Some(claims) => { let _ = CLAIMS.set(claims); true }
        None => false,
    }
}

/// The whole disk a device is on -- its handle -- and the stretch of it
/// the device is: its first sector there, and how many. None for a handle
/// that names no device.
fn extent(handle: usize) -> Option<(usize, u64, u64)> {
    let mut at = handle;
    let mut dev = device(at)?;
    let count = dev.capacity;
    let mut first = 0u64;
    /* Down to the disk. A disk was registered before any partition of it,
     * so the walk ends; the bound is the table's size all the same. */
    for _ in 0..MAX_DEVICES {
        match dev.backend {
            Backend::Driver(_) => return Some((at, first, count)),
            Backend::Partition { start } => {
                first = first.checked_add(start)?;
                at = dev.parent;
                dev = device(at)?;
            }
        }
    }
    None
}

/// Whether writing to one device can touch the other: whether the two are
/// stretches of one disk that share a sector -- the same device, a disk and
/// a partition of it, and two partitions a table has overlap, which it may:
/// the table is whoever wrote the disk's to choose. A device whose extent
/// cannot be worked out is taken to overlap everything.
fn overlap(a: usize, b: usize) -> bool {
    match (extent(a), extent(b)) {
        (Some((disk_a, first_a, count_a)), Some((disk_b, first_b, count_b))) => {
            disk_a == disk_b
                && first_a < first_b.saturating_add(count_b)
                && first_b < first_a.saturating_add(count_a)
        }
        _ => true,
    }
}

/// Claim a device against mounts, the disk log and other writers, naming
/// the holder a refusal reports: the claim, for `release` -- or who stands
/// in its way.
pub(crate) fn claim(handle: usize, holder: Holder, owner: Owner) -> Result<usize, Holder> {
    if handle == 0 || device(handle).is_none() {
        return Err(Holder::of(NO_DEVICE));
    }

    let mut claims = match CLAIMS.get() {
        Some(claims) => claims.lock(),
        None => return Err(Holder::of(NOT_READY)),
    };

    let mut free = None;
    for (slot, entry) in claims.entries.iter().enumerate() {
        if entry.claim == 0 {
            if free.is_none() {
                free = Some(slot);
            }
        } else if overlap(entry.device, handle) {
            return Err(entry.holder);
        }
    }

    let slot = free.ok_or(Holder::of(TOO_MANY))?;

    claims.generation += 1;
    let claim = (claims.generation << SLOT_BITS) | (slot + 1);
    claims.entries[slot] = ClaimEntry { device: handle, holder, claim, owner };
    Ok(claim)
}

/// The device a live claim is on: None for a word that is no claim's -- a
/// stale one, or one released. What a write is made under (W8): the claim
/// itself, not the device, so that a write without one cannot be asked.
pub(crate) fn claimed_device(claim: usize) -> Option<usize> {
    let slot = (claim & SLOT_MASK).checked_sub(1)?;
    let claims = CLAIMS.get()?.lock();
    let entry = claims.entries.get(slot)?;
    (entry.claim != 0 && entry.claim == claim).then_some(entry.device)
}

/// The claims the module called `name` took and never gave back, released:
/// how many. The loader's sweep (`rust_block_module_gone`), after the
/// module's queues have gone with theirs.
fn release_owned_by(name: &[u8]) -> usize {
    let Some(claims) = CLAIMS.get() else { return 0 };
    let mut claims = claims.lock();
    let mut released = 0;
    for entry in claims.entries.iter_mut() {
        if entry.claim != 0 && entry.owner.is_module(name) {
            *entry = NO_CLAIM;
            released += 1;
        }
    }
    released
}

/// Who holds a claim on the device or on anything overlapping it -- what a
/// claim of it would be refused for -- if anyone: asked under the claims'
/// lock, and nothing taken, so that a look gets in nobody's way.
pub(crate) fn in_the_way(handle: usize) -> Option<Holder> {
    let claims = CLAIMS.get()?.lock();
    claims.entries.iter().find(|e| e.claim != 0 && overlap(e.device, handle)).map(|e| e.holder)
}

/// Who holds a claim on the device itself -- not on one it overlaps -- if
/// anyone: copied under the claims' lock.
pub(crate) fn holder(handle: usize) -> Option<Holder> {
    let claims = CLAIMS.get()?.lock();
    claims.entries.iter().find(|e| e.claim != 0 && e.device == handle).map(|e| e.holder)
}

/// Give a claim back. A claim that is not the one the slot holds -- a stale
/// one, or one already released -- does nothing.
pub(crate) fn release(claim: usize) {
    let slot = claim & SLOT_MASK;
    if slot == 0 || slot > MAX_DEVICES {
        return;
    }

    if let Some(claims) = CLAIMS.get() {
        let mut claims = claims.lock();
        if claims.entries[slot - 1].claim == claim {
            claims.entries[slot - 1] = NO_CLAIM;
        }
    }
}

/* ---- what a module, and the boot path, call ----
 *
 * A module is linked on its own and binds these by name (`kcore::block::
 * Disk` is their wrapper); the kernel image itself comes in through `Disk`
 * in disk.rs and never through here. A device crosses as its handle, which
 * `device()` looks up, so any word will do; a buffer crosses as a pointer
 * and a count of sectors, which nothing here can check. */

/// The boot path, once interrupts and the scheduler run.
#[no_mangle]
pub extern "C" fn kernel_blockdev_set_interrupts_started() {
    INTERRUPTS_STARTED.store(true, Ordering::Release);
}

/// The device of that name, or 0.
///
/// # Safety
/// `name` points at `name_len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_blockdev_find(name: *const u8, name_len: usize) -> usize {
    if name.is_null() {
        return 0;
    }
    find(unsafe { core::slice::from_raw_parts(name, name_len) })
}

#[no_mangle]
pub extern "C" fn kernel_blockdev_capacity(handle: usize) -> u64 {
    capacity(handle)
}

#[no_mangle]
pub extern "C" fn kernel_blockdev_sector_size(handle: usize) -> u64 {
    sector_size(handle)
}

/// How many bytes `count` sectors of the device are.
fn byte_len(handle: usize, count: u32) -> Option<usize> {
    usize::try_from((count as u64).checked_mul(sector_size(handle))?).ok()
}

/// Synchronous read, count in sectors: 0 once the data is in buf.
///
/// # Safety
/// `buf` takes `count` sectors, and the device's driver may DMA into it.
#[no_mangle]
pub unsafe extern "C" fn kernel_blockdev_read(
    handle: usize, sector: u64, buf: *mut u8, count: u32,
) -> i32 {
    let len = match byte_len(handle, count) {
        Some(len) if !buf.is_null() => len,
        _ => return -1,
    };
    if read(handle, sector, unsafe { core::slice::from_raw_parts_mut(buf, len) }) { 0 } else { -1 }
}

/// Synchronous write under a claim -- the claim's word, not the device's:
/// a write was once asked of any device, and "writing needs a claim" was a
/// comment -- count in sectors: 0 once the device has the data, -1 for a
/// word that is no live claim's too.
///
/// # Safety
/// `buf` holds `count` sectors, and the device's driver may DMA out of it.
#[no_mangle]
pub unsafe extern "C" fn kernel_blockdev_write(
    claim: usize, sector: u64, buf: *const u8, count: u32, fua: i32,
) -> i32 {
    let Some(handle) = claimed_device(claim) else { return -1 };
    let len = match byte_len(handle, count) {
        Some(len) if !buf.is_null() => len,
        _ => return -1,
    };
    if write(handle, sector, unsafe { core::slice::from_raw_parts(buf, len) }, fua != 0) { 0 } else { -1 }
}

#[no_mangle]
pub extern "C" fn kernel_blockdev_flush(handle: usize) -> i32 {
    if flush(handle) { 0 } else { -1 }
}

/// 1 if the device has the asynchronous path.
#[no_mangle]
pub extern "C" fn kernel_blockdev_can_submit(handle: usize) -> i32 {
    can_submit(handle) as i32
}

fn submit_code(handle: usize, io: &BlockIo, kick_now: bool) -> i32 {
    match submit(handle, io, kick_now) {
        Ok(()) => SUBMIT_OK,
        Err(SubmitError::Busy) => SUBMIT_BUSY,
        Err(SubmitError::Invalid) => SUBMIT_INVALID,
        Err(SubmitError::Unsupported) => SUBMIT_UNSUPPORTED,
    }
}

/// Hand the device a read straight into physical memory -- a read alone:
/// a write or a flush is a claim's (`kernel_blockdev_submit_claimed`), and
/// refused here. Never blocks; `io` is read before this returns.
///
/// # Safety
/// `io` points at a valid BlockIo whose memory stays valid until its `done`
/// has run.
#[no_mangle]
pub unsafe extern "C" fn kernel_blockdev_submit(
    handle: usize, io: *const BlockIo, kick_now: i32,
) -> i32 {
    match unsafe { io.as_ref() } {
        Some(io) if io.op == IO_READ => submit_code(handle, io, kick_now != 0),
        _ => SUBMIT_INVALID,
    }
}

/// The same under a claim, for any I/O -- a write and a flush among them:
/// SUBMIT_INVALID for a word that is no live claim's.
///
/// # Safety
/// As `kernel_blockdev_submit`'s.
#[no_mangle]
pub unsafe extern "C" fn kernel_blockdev_submit_claimed(
    claim: usize, io: *const BlockIo, kick_now: i32,
) -> i32 {
    let (Some(handle), Some(io)) = (claimed_device(claim), unsafe { io.as_ref() }) else { return SUBMIT_INVALID };
    submit_code(handle, io, kick_now != 0)
}

/// Ring the doorbell for what a submit without a kick left queued.
#[no_mangle]
pub extern "C" fn kernel_blockdev_kick(handle: usize) {
    kick(handle);
}

/// How many partitions of the device the kernel found.
#[no_mangle]
pub extern "C" fn kernel_blockdev_partitions(handle: usize) -> u32 {
    partitions(handle)
}

/// `src` into the `cap` bytes at `out`, as many as fit: how many `src` has.
///
/// # Safety
/// `out`, unless null, takes `cap` bytes.
pub(crate) unsafe fn copy_out(src: &[u8], out: *mut u8, cap: usize) -> usize {
    if !out.is_null() {
        let n = src.len().min(cap);
        /* `out` takes `cap` bytes, and `n` is no more than that. */
        unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), out, n) };
    }
    src.len()
}

/// The claim a module takes when it writes to a device of its own accord,
/// held by `name` -- copied, cut at HOLDER_MAX bytes; "a module writing to
/// it" when it gives none: the claim for `kernel_blockdev_release`, or 0
/// with who holds an overlapping one copied into `refused`, its length --
/// HOLDER_MAX at most -- into `*refused_len`.
///
/// # Safety
/// `name` points at `name_len` readable bytes; `refused`, unless null,
/// takes `cap` bytes; `refused_len`, unless null, is writable.
#[no_mangle]
pub unsafe extern "C" fn kernel_blockdev_claim(
    owner: usize, handle: usize, name: *const u8, name_len: usize, refused: *mut u8, cap: usize,
    refused_len: *mut usize,
) -> usize {
    let holder = if name.is_null() || name_len == 0 {
        Holder::of(MODULE_HOLDER)
    } else {
        Holder::named(unsafe { core::slice::from_raw_parts(name, name_len) })
    };
    match claim(handle, holder, Owner::at(owner)) {
        Ok(claim) => claim,
        Err(in_the_way) => {
            let n = unsafe { copy_out(in_the_way.as_bytes(), refused, cap) };
            if let Some(len) = unsafe { refused_len.as_mut() } {
                *len = n.min(cap);
            }
            0
        }
    }
}

/// How many devices the table holds: what `kernel_blockdev_at` counts to.
#[no_mangle]
pub extern "C" fn kernel_blockdev_count() -> u32 {
    count()
}

/// The index'th device's handle, or 0.
#[no_mangle]
pub extern "C" fn kernel_blockdev_at(index: u32) -> usize {
    at(index)
}

/// The disk a partition is on, or 0 for a whole disk.
#[no_mangle]
pub extern "C" fn kernel_blockdev_parent(handle: usize) -> usize {
    parent(handle)
}

/// The sector of its disk a partition starts at; u64::MAX for a whole disk.
#[no_mangle]
pub extern "C" fn kernel_blockdev_start(handle: usize) -> u64 {
    start(handle).unwrap_or(u64::MAX)
}

/// The device's name into `out`, as much of it as fits in `cap` bytes, no
/// NUL: how long it is -- 0 for a handle that names nothing.
///
/// # Safety
/// `out`, unless null, takes `cap` bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_blockdev_name(handle: usize, out: *mut u8, cap: usize) -> usize {
    match device(handle) {
        Some(dev) => unsafe { copy_out(dev.name(), out, cap) },
        None => 0,
    }
}

/// What the device is found by again at the next boot -- `part:...` or
/// `disk:...` -- into `out` as `kernel_blockdev_name`'s: how long it is, 0
/// when it has none.
///
/// # Safety
/// `out`, unless null, takes `cap` bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_blockdev_ident(handle: usize, out: *mut u8, cap: usize) -> usize {
    match ident(handle) {
        Some(id) => unsafe { copy_out(id.as_bytes(), out, cap) },
        None => 0,
    }
}

/// The device whose id is the `len` bytes at `id`: its handle -- or 0, with
/// `*count`, unless null, set to how many have it: none, or more than one,
/// none of which is taken for it.
///
/// # Safety
/// `id` points at `len` readable bytes; `count`, unless null, is writable.
#[no_mangle]
pub unsafe extern "C" fn kernel_blockdev_find_ident(id: *const u8, len: usize, count: *mut u32) -> usize {
    if id.is_null() {
        return 0;
    }
    let (handle, n) = match find_ident(unsafe { core::slice::from_raw_parts(id, len) }) {
        Found::None => (0, 0),
        Found::One(handle) => (handle, 1),
        Found::Many(n) => (0, n),
    };
    if let Some(count) = unsafe { count.as_mut() } {
        *count = n;
    }
    handle
}

/// What the device's partition table says it is, into the 16 bytes at
/// `out`: 1 with its GPT type GUID there, as the entry has it; 2 with its
/// MBR type byte in the first; 0 for a whole disk, and -1 for a handle that
/// names nothing.
///
/// # Safety
/// `out`, unless null, takes 16 bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_blockdev_kind(handle: usize, out: *mut u8) -> i32 {
    if device(handle).is_none() {
        return -1;
    }
    match kind(handle) {
        PartKind::Disk => 0,
        PartKind::Gpt(guid) => {
            unsafe { copy_out(&guid, out, 16) };
            1
        }
        PartKind::Mbr(byte) => {
            unsafe { copy_out(&[byte], out, 16) };
            2
        }
    }
}

/// Who holds a claim on the device itself -- not on one it overlaps -- into
/// `out`, as `kernel_blockdev_name`'s: the name's length, 0 for nobody.
///
/// # Safety
/// `out`, unless null, takes `cap` bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_blockdev_holder(handle: usize, out: *mut u8, cap: usize) -> usize {
    match holder(handle) {
        Some(h) => unsafe { copy_out(h.as_bytes(), out, cap) },
        None => 0,
    }
}

/// Who holds a claim on the device or anything overlapping it -- what a
/// claim of it would be refused for -- into `out`, as
/// `kernel_blockdev_name`'s: the name's length, 0 for nobody. Nothing is
/// claimed by asking.
///
/// # Safety
/// `out`, unless null, takes `cap` bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_blockdev_in_the_way(handle: usize, out: *mut u8, cap: usize) -> usize {
    match in_the_way(handle) {
        Some(h) => unsafe { copy_out(h.as_bytes(), out, cap) },
        None => 0,
    }
}

/// 1 when writing to one of the two devices can touch the other: one
/// device, a disk and a partition of it, partitions that overlap -- and a
/// handle that names nothing, which is taken to overlap everything.
#[no_mangle]
pub extern "C" fn kernel_blockdev_overlaps(a: usize, b: usize) -> i32 {
    i32::from(overlaps(a, b))
}

/// 1 when the device takes `kernel_blockdev_discard`.
#[no_mangle]
pub extern "C" fn kernel_blockdev_can_discard(handle: usize) -> i32 {
    i32::from(can_discard(handle))
}

/// The `count` sectors from `sector` given back to the device -- inside it,
/// or refused: 0 once it has been told, -1 when it failed, takes no such
/// thing, or `claim` is no live claim's. A writer's call, as a write is:
/// made under the claim, by its word.
#[no_mangle]
pub extern "C" fn kernel_blockdev_discard(claim: usize, sector: u64, count: u64) -> i32 {
    let Some(handle) = claimed_device(claim) else { return -1 };
    if discard(handle, sector, count) { 0 } else { -1 }
}

/// What a module that went left open on the block layer, closed: the device
/// queues its code opened, and then the claims it took and never gave back
/// -- a device it held stayed "held by" a name nothing answered to, its
/// next load refused it -- each said as the module's bug. The loader's, at
/// the module's take-down (`ModuleTable::take_down`). Task context: a
/// queue's close waits for its device.
///
/// # Safety
/// `name` points at `len` bytes.
#[no_mangle]
pub unsafe extern "C" fn rust_block_module_gone(name: *const u8, len: usize) {
    if name.is_null() || len == 0 || len > kcore::owner::NAME_MAX {
        return;
    }
    let name = unsafe { core::slice::from_raw_parts(name, len) };
    let shown = core::str::from_utf8(name).unwrap_or("?");
    let queues = crate::devq::close_owned_by(name);
    if queues != 0 {
        klog!(0, "module: {} left {} device queue(s) open: closed", shown, queues);
    }
    let claims = release_owned_by(name);
    if claims != 0 {
        klog!(0, "module: {} left {} device claim(s) held: released", shown, claims);
    }
}

#[no_mangle]
pub extern "C" fn kernel_blockdev_release(claim: usize) {
    release(claim);
}

/* ---- the `disks` command ---- */

pub fn dump(_args: &str, out: &mut Output) {
    let devices = count();
    if devices == 0 {
        let _ = writeln!(out, "no block devices");
        return;
    }

    for index in 0..devices {
        let handle = at(index);
        let dev = match device(handle) {
            Some(dev) => dev,
            None => continue,
        };

        let bytes = dev.capacity.saturating_mul(dev.sector_size);
        let _ = write!(out, "{}  {} sectors ({} MB)  {} bytes/sector",
            core::str::from_utf8(dev.name()).unwrap_or("?"),
            dev.capacity, bytes / (1024 * 1024), dev.sector_size);
        let _ = match dev.ident.as_deref() {
            Some(id) => writeln!(out, "  {}", id),
            None => writeln!(out),
        };
    }
}
