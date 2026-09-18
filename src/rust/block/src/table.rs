//! The kernel's block device table: every disk and partition the kernel has,
//! what `disks` lists, and the claims that keep two writers off the same
//! sectors.
//!
//! This is the C ABI half of the block layer -- the `kernel_blockdev_*`
//! functions both the C++ side (block/block_device.h) and the Rust side
//! (`kcore::block`, and through it every module) call. A device is a handle,
//! and a handle is a slot in the table plus one, so zero is never a device.
//!
//! The table only grows: nothing unregisters, which is what makes a lookup
//! lock-free -- a slot is written once, with a release, and read with an
//! acquire. The claims are the one part that needs a lock, and take the
//! kernel's spinlock through `kcore`.

use alloc::boxed::Box;
use core::cell::UnsafeCell;
use core::fmt::Write;
use core::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

use ffi::block::{BlockDeviceOps, BlockIo};
use kcore::cmd::Output;
use kcore::sync::SpinLock;
use kcore::trace;

/// What the table holds. The C++ side mirrors it as
/// BlockDeviceTable::MaxDevices.
pub const MAX_DEVICES: usize = 48;

/// What a driver's submit answers (kcore::block, block/block_device.h)
const SUBMIT_INVALID: i32 = 2;
const SUBMIT_UNSUPPORTED: i32 = 3;

/// A registered device: the ops table it gave, kept for the life of the
/// kernel because nothing takes a device back.
struct Device {
    ops: BlockDeviceOps,
}

/* The ops are only read after registration, and every pointer in them is the
 * registrant's to keep valid -- which is the contract of registering. */
unsafe impl Sync for Device {}
unsafe impl Send for Device {}

static DEVICES: [AtomicPtr<Device>; MAX_DEVICES] = {
    const NULL: AtomicPtr<Device> = AtomicPtr::new(core::ptr::null_mut());
    [NULL; MAX_DEVICES]
};
static COUNT: AtomicU32 = AtomicU32::new(0);

fn device(handle: usize) -> Option<&'static Device> {
    if handle == 0 || handle > MAX_DEVICES {
        return None;
    }

    let ptr = DEVICES[handle - 1].load(Ordering::Acquire);
    if ptr.is_null() {
        None
    } else {
        Some(unsafe { &*ptr })
    }
}

fn name_of(dev: &Device) -> &'static [u8] {
    if dev.ops.name.is_null() {
        return b"";
    }

    /* The name is the registrant's, NUL-terminated and kept for good. */
    let mut len = 0;
    while unsafe { *dev.ops.name.add(len) } != 0 {
        len += 1;
    }
    unsafe { core::slice::from_raw_parts(dev.ops.name, len) }
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

/// Register a device. The ops are copied; everything they point at -- the
/// name, the context -- stays the registrant's, and has to outlive the
/// kernel's use of it, which is to say the kernel.
///
/// # Safety
/// `ops` points at a valid BlockDeviceOps for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn kernel_blockdev_register(ops: *const BlockDeviceOps) -> usize {
    if ops.is_null() {
        return 0;
    }

    let ops = unsafe { core::ptr::read(ops) };
    if ops.name.is_null() || ops.read_sectors.is_none() || ops.write_sectors.is_none() {
        return 0;
    }

    /* The asynchronous path is both or neither: a submit that may leave its
     * doorbell owed, with no kick to ring it, would queue commands that never
     * reach the device. */
    if ops.submit.is_none() != ops.kick.is_none() {
        return 0;
    }

    let slot = match reserve() {
        Some(slot) => slot,
        None => {
            trace!(0, "block: the device table is full at {}", MAX_DEVICES);
            return 0;
        }
    };

    let dev = Box::into_raw(Box::new(Device { ops }));
    DEVICES[slot].store(dev, Ordering::Release);

    let dev = unsafe { &*dev };
    trace!(0, "block: {} registered, {} sectors of {} bytes",
        core::str::from_utf8(name_of(dev)).unwrap_or("?"),
        dev.ops.capacity, dev.ops.sector_size);

    slot + 1
}

/// How many devices the table holds. It only grows, so an index once valid
/// stays valid and names the same device.
#[no_mangle]
pub extern "C" fn kernel_blockdev_count() -> u32 {
    COUNT.load(Ordering::Acquire)
}

/// The index'th device, or 0. A slot being reserved by a registration that
/// has not finished reads as 0, and the caller skips it.
#[no_mangle]
pub extern "C" fn kernel_blockdev_at(index: u32) -> usize {
    let index = index as usize;
    if index >= MAX_DEVICES || DEVICES[index].load(Ordering::Acquire).is_null() {
        0
    } else {
        index + 1
    }
}

/// The device of that name, or 0.
///
/// # Safety
/// `name` points at `name_len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_blockdev_find(name: *const u8, name_len: usize) -> usize {
    if name.is_null() || name_len == 0 {
        return 0;
    }

    let wanted = unsafe { core::slice::from_raw_parts(name, name_len) };
    for index in 0..kernel_blockdev_count() {
        let handle = kernel_blockdev_at(index) ;
        match device(handle) {
            Some(dev) if name_of(dev) == wanted => return handle,
            _ => continue,
        }
    }
    0
}

/// The device's name, NUL-terminated and the registrant's to keep. Null for
/// a handle that names nothing.
#[no_mangle]
pub extern "C" fn kernel_blockdev_name_ptr(handle: usize) -> *const u8 {
    match device(handle) {
        Some(dev) => dev.ops.name,
        None => core::ptr::null(),
    }
}

/// The device's name into buf, NUL-terminated: the length written, or 0 if
/// it does not fit.
///
/// # Safety
/// `buf` points at `len` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn kernel_blockdev_name(handle: usize, buf: *mut u8, len: usize) -> usize {
    let dev = match device(handle) {
        Some(dev) => dev,
        None => return 0,
    };

    let name = name_of(dev);
    if buf.is_null() || name.len() + 1 > len {
        return 0;
    }

    unsafe {
        core::ptr::copy_nonoverlapping(name.as_ptr(), buf, name.len());
        *buf.add(name.len()) = 0;
    }
    name.len()
}

/// The disk a partition is on, or 0 for a whole disk.
#[no_mangle]
pub extern "C" fn kernel_blockdev_parent(handle: usize) -> usize {
    device(handle).map_or(0, |dev| dev.ops.parent)
}

#[no_mangle]
pub extern "C" fn kernel_blockdev_capacity(handle: usize) -> u64 {
    device(handle).map_or(0, |dev| dev.ops.capacity)
}

#[no_mangle]
pub extern "C" fn kernel_blockdev_sector_size(handle: usize) -> u64 {
    device(handle).map_or(0, |dev| dev.ops.sector_size)
}

/// Synchronous read, count in sectors: 0 once the data is in buf.
///
/// # Safety
/// `buf` takes `count` sectors, and the device's driver may DMA into it.
#[no_mangle]
pub unsafe extern "C" fn kernel_blockdev_read(
    handle: usize, sector: u64, buf: *mut u8, count: u32,
) -> i32 {
    match device(handle) {
        Some(dev) => match dev.ops.read_sectors {
            Some(read) => read(dev.ops.ctx, sector, buf, count),
            None => -1,
        },
        None => -1,
    }
}

/// Synchronous write, count in sectors: 0 once the device has the data.
///
/// # Safety
/// `buf` holds `count` sectors, and the device's driver may DMA out of it.
#[no_mangle]
pub unsafe extern "C" fn kernel_blockdev_write(
    handle: usize, sector: u64, buf: *const u8, count: u32, fua: i32,
) -> i32 {
    match device(handle) {
        Some(dev) => match dev.ops.write_sectors {
            Some(write) => write(dev.ops.ctx, sector, buf, count, fua),
            None => -1,
        },
        None => -1,
    }
}

#[no_mangle]
pub extern "C" fn kernel_blockdev_flush(handle: usize) -> i32 {
    match device(handle) {
        /* A device with no write cache to push has nothing to do here. */
        Some(dev) => dev.ops.flush.map_or(0, |flush| flush(dev.ops.ctx)),
        None => -1,
    }
}

/// 1 if the device has the asynchronous path.
#[no_mangle]
pub extern "C" fn kernel_blockdev_can_submit(handle: usize) -> i32 {
    match device(handle) {
        Some(dev) => dev.ops.submit.is_some() as i32,
        None => 0,
    }
}

/// Hand the device an I/O straight to or from physical memory. Never blocks;
/// `io` is read before this returns.
///
/// # Safety
/// `io` points at a valid BlockIo whose memory stays valid until its `done`
/// has run.
#[no_mangle]
pub unsafe extern "C" fn kernel_blockdev_submit(
    handle: usize, io: *const BlockIo, kick: i32,
) -> i32 {
    if io.is_null() {
        return SUBMIT_INVALID;
    }

    match device(handle) {
        Some(dev) => match dev.ops.submit {
            Some(submit) => submit(dev.ops.ctx, io, kick),
            None => SUBMIT_UNSUPPORTED,
        },
        None => SUBMIT_INVALID,
    }
}

/// Ring the doorbell for what a submit without a kick left queued.
#[no_mangle]
pub extern "C" fn kernel_blockdev_kick(handle: usize) {
    if let Some(dev) = device(handle) {
        if let Some(kick) = dev.ops.kick {
            kick(dev.ops.ctx);
        }
    }
}

/// How many partitions of the device the kernel found.
#[no_mangle]
pub extern "C" fn kernel_blockdev_partitions(handle: usize) -> u32 {
    if handle == 0 {
        return 0;
    }

    let mut found = 0;
    for index in 0..kernel_blockdev_count() {
        let other = kernel_blockdev_at(index);
        if other != 0 && kernel_blockdev_parent(other) == handle {
            found += 1;
        }
    }
    found
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
    /// NUL-terminated, the claimant's to keep, and only read while the claim
    /// stands
    holder: *const u8,
    /// 0: the slot is free
    claim: usize,
}

struct ClaimTable {
    lock: SpinLock,
    entries: UnsafeCell<[ClaimEntry; MAX_DEVICES]>,
    generation: UnsafeCell<usize>,
}

/* Everything inside is touched with the lock held. */
unsafe impl Sync for ClaimTable {}
unsafe impl Send for ClaimTable {}

static CLAIMS: AtomicPtr<ClaimTable> = AtomicPtr::new(core::ptr::null_mut());

const TOO_MANY: &[u8] = b"too many claims already\0";
const NOT_READY: &[u8] = b"the block layer, still starting up\0";
const NO_DEVICE: &[u8] = b"nothing -- there is no such device\0";
const MODULE_HOLDER: &[u8] = b"a module writing to it\0";

/// Called once, from `block::init`, before anything can claim: the table
/// needs the kernel's spinlock, which is a handle and cannot be a static.
pub fn claims_setup() -> bool {
    if !CLAIMS.load(Ordering::Acquire).is_null() {
        return true;
    }

    let lock = match SpinLock::new() {
        Some(lock) => lock,
        None => return false,
    };

    let table = Box::into_raw(Box::new(ClaimTable {
        lock,
        entries: UnsafeCell::new([ClaimEntry { device: 0, holder: core::ptr::null(), claim: 0 };
            MAX_DEVICES]),
        generation: UnsafeCell::new(0),
    }));
    CLAIMS.store(table, Ordering::Release);
    true
}

/// Whether writing to one device can touch the other: the same device, or a
/// disk and a partition of it.
fn overlap(a: usize, b: usize) -> bool {
    let mut walk = a;
    while walk != 0 {
        if walk == b {
            return true;
        }
        walk = kernel_blockdev_parent(walk);
    }

    let mut walk = b;
    while walk != 0 {
        if walk == a {
            return true;
        }
        walk = kernel_blockdev_parent(walk);
    }

    false
}

/// Claim a device against mounts, the disk log and other writers. Returns the
/// claim for `kernel_blockdev_release`, or 0 with `held_by` set to who holds
/// an overlapping one -- a NUL-terminated name the kernel keeps.
///
/// # Safety
/// `holder` is NUL-terminated and outlives the claim; `held_by`, if given, is
/// writable.
#[no_mangle]
pub unsafe extern "C" fn kernel_blockdev_claim_as(
    handle: usize, holder: *const u8, held_by: *mut *const u8,
) -> usize {
    let refuse = |why: &'static [u8]| -> usize {
        if !held_by.is_null() {
            unsafe { *held_by = why.as_ptr() };
        }
        0
    };

    if handle == 0 || holder.is_null() {
        return refuse(NO_DEVICE);
    }

    let table = CLAIMS.load(Ordering::Acquire);
    if table.is_null() {
        return refuse(NOT_READY);
    }
    let table = unsafe { &*table };

    let _guard = table.lock.lock();
    let entries = unsafe { &mut *table.entries.get() };

    let mut free = None;
    for (slot, entry) in entries.iter().enumerate() {
        if entry.claim == 0 {
            if free.is_none() {
                free = Some(slot);
            }
        } else if overlap(entry.device, handle) {
            let held = entry.holder;
            if !held_by.is_null() {
                unsafe { *held_by = held };
            }
            return 0;
        }
    }

    let slot = match free {
        Some(slot) => slot,
        None => return refuse(TOO_MANY),
    };

    let generation = unsafe { &mut *table.generation.get() };
    *generation += 1;

    let claim = (*generation << SLOT_BITS) | (slot + 1);
    entries[slot] = ClaimEntry { device: handle, holder, claim };
    claim
}

/// The claim a module takes when it writes to a device of its own accord.
///
/// # Safety
/// `held_by`, if given, is writable.
#[no_mangle]
pub unsafe extern "C" fn kernel_blockdev_claim(handle: usize, held_by: *mut *const u8) -> usize {
    unsafe { kernel_blockdev_claim_as(handle, MODULE_HOLDER.as_ptr(), held_by) }
}

/// Give a claim back. A claim that is not the one the slot holds -- a stale
/// one, or one already released -- does nothing.
#[no_mangle]
pub extern "C" fn kernel_blockdev_release(claim: usize) {
    let slot = claim & SLOT_MASK;
    if slot == 0 || slot > MAX_DEVICES {
        return;
    }

    let table = CLAIMS.load(Ordering::Acquire);
    if table.is_null() {
        return;
    }
    let table = unsafe { &*table };

    let _guard = table.lock.lock();
    let entries = unsafe { &mut *table.entries.get() };
    if entries[slot - 1].claim == claim {
        entries[slot - 1] = ClaimEntry { device: 0, holder: core::ptr::null(), claim: 0 };
    }
}

/* ---- the `disks` command ---- */

pub fn dump(_args: &str, out: &mut Output) {
    let devices = kernel_blockdev_count();
    if devices == 0 {
        let _ = writeln!(out, "no block devices");
        return;
    }

    for index in 0..devices {
        let handle = kernel_blockdev_at(index);
        let dev = match device(handle) {
            Some(dev) => dev,
            None => continue,
        };

        let bytes = dev.ops.capacity.saturating_mul(dev.ops.sector_size);
        let _ = writeln!(out, "{}  {} sectors ({} MB)  {} bytes/sector",
            core::str::from_utf8(name_of(dev)).unwrap_or("?"),
            dev.ops.capacity, bytes / (1024 * 1024), dev.ops.sector_size);
    }
}
