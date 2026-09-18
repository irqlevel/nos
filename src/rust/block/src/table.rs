//! The kernel's block device table: every disk and partition the kernel has,
//! what `disks` lists, and the claims that keep two writers off the same
//! sectors.
//!
//! This is the C ABI half of the block layer -- the `kernel_blockdev_*`
//! functions both the C++ side (block/block.h) and the Rust side
//! (`kcore::block`, and through it every module) call. A device is a handle,
//! and a handle is a slot in the table plus one, so zero is never a device.
//! There is no C++ view of a device: the shell, the disk log and a mount
//! hold that handle and nothing else.
//!
//! The table only grows: nothing unregisters, which is what makes a lookup
//! lock-free -- a slot is filled once and only read after. The claims are the
//! one part that needs a lock, and sit inside the kernel's spinlock.

use alloc::boxed::Box;
use core::ffi::CStr;
use core::fmt::Write;
use core::sync::atomic::{AtomicU32, Ordering};

use ffi::block::{BlockDeviceOps, BlockIo};
use kcore::cmd::Output;
use kcore::once::{Once, OnceBox};
use kcore::sync::SpinLock;
use kcore::trace;

/// What the table holds.
pub const MAX_DEVICES: usize = 48;

/// What a driver's submit answers (kcore::block)
const SUBMIT_INVALID: i32 = 2;
const SUBMIT_UNSUPPORTED: i32 = 3;

/// A registered device: what its ops table said, kept for the life of the
/// kernel because nothing takes a device back.
///
/// The name is a copy, and the driver's context is kept as the word it is to
/// this layer -- handed back on every call and never looked into. So a
/// device is plain data and a few functions, and may be shared between CPUs
/// without anyone having to promise anything.
struct Device {
    /// With its NUL, which is how `kernel_blockdev_name` hands it out
    name: Box<CStr>,
    capacity: u64,
    sector_size: u64,
    read_sectors: extern "C" fn(ctx: *mut u8, sector: u64, buf: *mut u8, count: u32) -> i32,
    write_sectors: extern "C" fn(
        ctx: *mut u8, sector: u64, buf: *const u8, count: u32, fua: i32,
    ) -> i32,
    flush: Option<extern "C" fn(ctx: *mut u8) -> i32>,
    /// The asynchronous path, both halves or neither
    submit: Option<(
        extern "C" fn(ctx: *mut u8, io: *const BlockIo, kick: i32) -> i32,
        extern "C" fn(ctx: *mut u8),
    )>,
    ctx: usize,
    parent: usize,
}

impl Device {
    fn name(&self) -> &[u8] {
        self.name.to_bytes()
    }

    fn ctx(&self) -> *mut u8 {
        self.ctx as *mut u8
    }
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

/// Register a device. The ops are copied, and so is the name; the context
/// stays the registrant's, and has to outlive the kernel's use of it, which
/// is to say the kernel.
///
/// # Safety
/// `ops` points at a valid BlockDeviceOps for the duration of the call, and
/// its name is NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn kernel_blockdev_register(ops: *const BlockDeviceOps) -> usize {
    let ops = match unsafe { ops.as_ref() } {
        Some(ops) => ops,
        None => return 0,
    };

    let (read_sectors, write_sectors) = match (ops.read_sectors, ops.write_sectors) {
        (Some(read), Some(write)) if !ops.name.is_null() => (read, write),
        _ => return 0,
    };

    /* The asynchronous path is both or neither: a submit that may leave its
     * doorbell owed, with no kick to ring it, would queue commands that never
     * reach the device. */
    let submit = match (ops.submit, ops.kick) {
        (Some(submit), Some(kick)) => Some((submit, kick)),
        (None, None) => None,
        _ => return 0,
    };

    let name: Box<CStr> = unsafe { CStr::from_ptr(ops.name.cast()) }.into();

    let slot = match reserve() {
        Some(slot) => slot,
        None => {
            trace!(0, "block: the device table is full at {}", MAX_DEVICES);
            return 0;
        }
    };

    let made = DEVICES[slot].get_or_try_init(|| Some(Box::new(Device {
        name,
        capacity: ops.capacity,
        sector_size: ops.sector_size,
        read_sectors,
        write_sectors,
        flush: ops.flush,
        submit,
        ctx: ops.ctx as usize,
        parent: ops.parent,
    })));

    if let Some(dev) = made {
        trace!(0, "block: {} registered, {} sectors of {} bytes",
            core::str::from_utf8(dev.name()).unwrap_or("?"), dev.capacity, dev.sector_size);
    }

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
    match DEVICES.get(index) {
        Some(slot) if slot.get().is_some() => index + 1,
        _ => 0,
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
        let handle = kernel_blockdev_at(index);
        match device(handle) {
            Some(dev) if dev.name() == wanted => return handle,
            _ => continue,
        }
    }
    0
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

    let name = dev.name.to_bytes_with_nul();
    if buf.is_null() || name.len() > len {
        return 0;
    }

    unsafe { core::ptr::copy_nonoverlapping(name.as_ptr(), buf, name.len()) };
    name.len() - 1
}

/// The disk a partition is on, or 0 for a whole disk.
#[no_mangle]
pub extern "C" fn kernel_blockdev_parent(handle: usize) -> usize {
    device(handle).map_or(0, |dev| dev.parent)
}

#[no_mangle]
pub extern "C" fn kernel_blockdev_capacity(handle: usize) -> u64 {
    device(handle).map_or(0, |dev| dev.capacity)
}

#[no_mangle]
pub extern "C" fn kernel_blockdev_sector_size(handle: usize) -> u64 {
    device(handle).map_or(0, |dev| dev.sector_size)
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
        Some(dev) => (dev.read_sectors)(dev.ctx(), sector, buf, count),
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
        Some(dev) => (dev.write_sectors)(dev.ctx(), sector, buf, count, fua),
        None => -1,
    }
}

#[no_mangle]
pub extern "C" fn kernel_blockdev_flush(handle: usize) -> i32 {
    match device(handle) {
        /* A device with no write cache to push has nothing to do here. */
        Some(dev) => dev.flush.map_or(0, |flush| flush(dev.ctx())),
        None => -1,
    }
}

/// 1 if the device has the asynchronous path.
#[no_mangle]
pub extern "C" fn kernel_blockdev_can_submit(handle: usize) -> i32 {
    match device(handle) {
        Some(dev) => dev.submit.is_some() as i32,
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
        Some(dev) => match dev.submit {
            Some((submit, _)) => submit(dev.ctx(), io, kick),
            None => SUBMIT_UNSUPPORTED,
        },
        None => SUBMIT_INVALID,
    }
}

/// Ring the doorbell for what a submit without a kick left queued.
#[no_mangle]
pub extern "C" fn kernel_blockdev_kick(handle: usize) {
    if let Some(dev) = device(handle) {
        if let Some((_, kick)) = dev.submit {
            kick(dev.ctx());
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

/// Set once interrupts and the scheduler are running (the boot path calls
/// it). Before that a synchronous I/O has to poll its device: there is
/// nothing yet to wake a waiter.
static INTERRUPTS_STARTED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

#[no_mangle]
pub extern "C" fn kernel_blockdev_set_interrupts_started() {
    INTERRUPTS_STARTED.store(true, Ordering::Release);
}

#[no_mangle]
pub extern "C" fn kernel_blockdev_interrupts_started() -> i32 {
    INTERRUPTS_STARTED.load(Ordering::Acquire) as i32
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
    /// Where the claimant's name is: NUL-terminated, the claimant's to keep,
    /// and only ever handed back -- to whoever is refused because of this
    /// claim -- never read here.
    holder: usize,
    /// 0: the slot is free
    claim: usize,
}

const NO_CLAIM: ClaimEntry = ClaimEntry { device: 0, holder: 0, claim: 0 };

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

/// The claim, or where the name of whoever stands in its way is.
fn claim(handle: usize, holder: usize) -> Result<usize, usize> {
    if handle == 0 || holder == 0 {
        return Err(NO_DEVICE.as_ptr() as usize);
    }

    let mut claims = match CLAIMS.get() {
        Some(claims) => claims.lock(),
        None => return Err(NOT_READY.as_ptr() as usize),
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

    let slot = free.ok_or(TOO_MANY.as_ptr() as usize)?;

    claims.generation += 1;
    let claim = (claims.generation << SLOT_BITS) | (slot + 1);
    claims.entries[slot] = ClaimEntry { device: handle, holder, claim };
    Ok(claim)
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
    match claim(handle, holder as usize) {
        Ok(claim) => claim,
        Err(in_the_way) => {
            if let Some(held_by) = unsafe { held_by.as_mut() } {
                *held_by = in_the_way as *const u8;
            }
            0
        }
    }
}

/// The claim a module takes when it writes to a device of its own accord.
///
/// # Safety
/// `held_by`, if given, is writable.
#[no_mangle]
pub unsafe extern "C" fn kernel_blockdev_claim(handle: usize, held_by: *mut *const u8) -> usize {
    unsafe { kernel_blockdev_claim_as(handle, MODULE_HOLDER.as_ptr().cast(), held_by) }
}

/// Give a claim back. A claim that is not the one the slot holds -- a stale
/// one, or one already released -- does nothing.
#[no_mangle]
pub extern "C" fn kernel_blockdev_release(claim: usize) {
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

        let bytes = dev.capacity.saturating_mul(dev.sector_size);
        let _ = writeln!(out, "{}  {} sectors ({} MB)  {} bytes/sector",
            core::str::from_utf8(dev.name()).unwrap_or("?"),
            dev.capacity, bytes / (1024 * 1024), dev.sector_size);
    }
}
