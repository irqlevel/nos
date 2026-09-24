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

use alloc::boxed::Box;
use alloc::ffi::CString;
use core::ffi::CStr;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use ffi::block::BlockIo;
use kcore::block::{SubmitError, IO_FLUSH, SUBMIT_BUSY, SUBMIT_INVALID, SUBMIT_OK, SUBMIT_UNSUPPORTED};
use kcore::cmd::Output;
use kcore::consts::PAGE_SIZE;
use kcore::dma::DmaBuffer;
use kcore::once::{Once, OnceBox};
use kcore::sync::SpinLock;
use kcore::trace;

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
            trace!(0, "block: the device table is full at {}", MAX_DEVICES);
            return 0;
        }
    };

    if let Some(dev) = DEVICES[slot].get_or_try_init(|| Some(Box::new(dev))) {
        trace!(0, "block: {} registered, {} sectors of {} bytes",
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
    })
}

/// One partition of the device `parent` names, as a device of its own. The
/// asynchronous path is there exactly when the disk has one -- a caller
/// picks its path by `can_submit`, and a partition that claimed one its disk
/// cannot serve would have every submission refused.
pub fn register_partition(parent: usize, start: u64, count: u64, name: &CStr) -> bool {
    let sector_size = match device(parent) {
        Some(disk) if disk.within(start, count) && count != 0 => disk.sector_size,
        _ => return false,
    };

    add(Device {
        name: name.into(),
        capacity: count,
        sector_size,
        parent,
        backend: Backend::Partition { start },
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

    match dev.backend {
        Backend::Driver(driver) => driver.read(sector, buf),
        Backend::Partition { start } if dev.within(sector, count) => {
            read(dev.parent, start + sector, buf)
        }
        Backend::Partition { .. } => false,
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

    match dev.backend {
        Backend::Driver(driver) => driver.write(sector, data, fua),
        Backend::Partition { start } if dev.within(sector, count) => {
            write(dev.parent, start + sector, data, fua)
        }
        Backend::Partition { .. } => false,
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
            trace!(0, "block: {}: a batch piece of {} bytes at {} of the buffer, to sector {}, is none a device takes",
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

pub(crate) fn can_submit(handle: usize) -> bool {
    match device(handle) {
        Some(dev) => match dev.backend {
            Backend::Driver(driver) => driver.is_async(),
            Backend::Partition { .. } => can_submit(dev.parent),
        },
        None => false,
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

    let start = match dev.backend {
        Backend::Driver(driver) if driver.is_async() => return driver.submit(io, kick_now),
        Backend::Driver(_) => return Err(SubmitError::Unsupported),
        Backend::Partition { start } => start,
    };

    /* A flush is about the device, not about a range of it: passed on as it
     * is. */
    if io.op == IO_FLUSH {
        return submit(dev.parent, io, kick_now);
    }

    if !dev.within(io.sector, io.count as u64) {
        /* Refused, but a kick is still a kick: what was queued before it
         * without a doorbell is owed one. */
        if kick_now {
            kick(dev.parent);
        }
        return Err(SubmitError::Invalid);
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

#[derive(Clone, Copy)]
struct ClaimEntry {
    device: usize,
    /// Who holds it: handed back to whoever is refused because of this
    /// claim.
    holder: &'static CStr,
    /// 0: the slot is free
    claim: usize,
}

const NO_CLAIM: ClaimEntry = ClaimEntry { device: 0, holder: c"", claim: 0 };

struct Claims {
    entries: [ClaimEntry; MAX_DEVICES],
    generation: usize,
}

static CLAIMS: Once<SpinLock<Claims>> = Once::new();

const TOO_MANY: &CStr = c"too many claims already";
const NOT_READY: &CStr = c"the block layer, still starting up";
const NO_DEVICE: &CStr = c"nothing -- there is no such device";
const MODULE_HOLDER: &CStr = c"a module writing to it";

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

/// Whether writing to one device can touch the other: the same device, or a
/// disk and a partition of it.
fn overlap(a: usize, b: usize) -> bool {
    let mut walk = a;
    while walk != 0 {
        if walk == b {
            return true;
        }
        walk = parent(walk);
    }

    let mut walk = b;
    while walk != 0 {
        if walk == a {
            return true;
        }
        walk = parent(walk);
    }

    false
}

/// Claim a device against mounts, the disk log and other writers, naming
/// the holder a refusal reports: the claim, for `release` -- or who stands
/// in its way.
pub(crate) fn claim(handle: usize, holder: &'static CStr) -> Result<usize, &'static CStr> {
    if handle == 0 {
        return Err(NO_DEVICE);
    }

    let mut claims = match CLAIMS.get() {
        Some(claims) => claims.lock(),
        None => return Err(NOT_READY),
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

    let slot = free.ok_or(TOO_MANY)?;

    claims.generation += 1;
    let claim = (claims.generation << SLOT_BITS) | (slot + 1);
    claims.entries[slot] = ClaimEntry { device: handle, holder, claim };
    Ok(claim)
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

/// Synchronous write, count in sectors: 0 once the device has the data.
///
/// # Safety
/// `buf` holds `count` sectors, and the device's driver may DMA out of it.
#[no_mangle]
pub unsafe extern "C" fn kernel_blockdev_write(
    handle: usize, sector: u64, buf: *const u8, count: u32, fua: i32,
) -> i32 {
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

/// Hand the device an I/O straight to or from physical memory. Never blocks;
/// `io` is read before this returns.
///
/// # Safety
/// `io` points at a valid BlockIo whose memory stays valid until its `done`
/// has run.
#[no_mangle]
pub unsafe extern "C" fn kernel_blockdev_submit(
    handle: usize, io: *const BlockIo, kick_now: i32,
) -> i32 {
    let io = match unsafe { io.as_ref() } {
        Some(io) => io,
        None => return SUBMIT_INVALID,
    };

    match submit(handle, io, kick_now != 0) {
        Ok(()) => SUBMIT_OK,
        Err(SubmitError::Busy) => SUBMIT_BUSY,
        Err(SubmitError::Invalid) => SUBMIT_INVALID,
        Err(SubmitError::Unsupported) => SUBMIT_UNSUPPORTED,
    }
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

/// The claim a module takes when it writes to a device of its own accord:
/// the claim for `kernel_blockdev_release`, or 0 with `held_by` set to who
/// holds an overlapping one -- a NUL-terminated name the kernel keeps.
///
/// # Safety
/// `held_by`, if given, is writable.
#[no_mangle]
pub unsafe extern "C" fn kernel_blockdev_claim(handle: usize, held_by: *mut *const u8) -> usize {
    match claim(handle, MODULE_HOLDER) {
        Ok(claim) => claim,
        Err(in_the_way) => {
            if let Some(held_by) = unsafe { held_by.as_mut() } {
                *held_by = in_the_way.as_ptr().cast();
            }
            0
        }
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
        let _ = writeln!(out, "{}  {} sectors ({} MB)  {} bytes/sector",
            core::str::from_utf8(dev.name()).unwrap_or("?"),
            dev.capacity, bytes / (1024 * 1024), dev.sector_size);
    }
}
