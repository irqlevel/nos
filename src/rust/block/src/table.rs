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

/// What a driver's submit answers, and the one op that is not about a range
/// (kcore::block)
const SUBMIT_INVALID: i32 = 2;
const SUBMIT_UNSUPPORTED: i32 = 3;
const IO_FLUSH: u8 = 2;

/// A registered device, kept for the life of the kernel because nothing
/// takes a device back.
///
/// The name is a copy, and a driver's context is kept as the word it is to
/// this layer -- handed back on every call and never looked into. So a
/// device is plain data and a few functions, and may be shared between CPUs
/// without anyone having to promise anything.
struct Device {
    /// With its NUL, which is how `kernel_blockdev_name` hands it out
    name: Box<CStr>,
    capacity: u64,
    sector_size: u64,
    /// The disk a partition is on, as its handle; 0 for a whole disk
    parent: usize,
    backend: Backend,
}

/// What does a device's I/O.
enum Backend {
    /// A driver: what its ops table said
    Driver {
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
    },
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
 * A buffer is a pointer here, passed on and never looked into: a driver
 * DMAs to it, and what it points at is the contract of whoever called in.
 * A partition is its disk a few sectors on, and a disk was registered before
 * any partition of it, so the walk up ends. */

fn read(handle: usize, sector: u64, buf: *mut u8, count: u32) -> i32 {
    let dev = match device(handle) {
        Some(dev) => dev,
        None => return -1,
    };
    match dev.backend {
        Backend::Driver { read_sectors, ctx, .. } => {
            read_sectors(ctx as *mut u8, sector, buf, count)
        }
        Backend::Partition { start } if dev.within(sector, count as u64) => {
            read(dev.parent, start + sector, buf, count)
        }
        Backend::Partition { .. } => -1,
    }
}

fn write(handle: usize, sector: u64, buf: *const u8, count: u32, fua: i32) -> i32 {
    let dev = match device(handle) {
        Some(dev) => dev,
        None => return -1,
    };
    match dev.backend {
        Backend::Driver { write_sectors, ctx, .. } => {
            write_sectors(ctx as *mut u8, sector, buf, count, fua)
        }
        Backend::Partition { start } if dev.within(sector, count as u64) => {
            write(dev.parent, start + sector, buf, count, fua)
        }
        Backend::Partition { .. } => -1,
    }
}

fn flush(handle: usize) -> i32 {
    match device(handle) {
        Some(dev) => match dev.backend {
            /* A device with no write cache to push has nothing to do here. */
            Backend::Driver { flush: driver_flush, ctx, .. } => {
                driver_flush.map_or(0, |driver_flush| driver_flush(ctx as *mut u8))
            }
            Backend::Partition { .. } => flush(dev.parent),
        },
        None => -1,
    }
}

fn can_submit(handle: usize) -> bool {
    match device(handle) {
        Some(dev) => match dev.backend {
            Backend::Driver { submit: async_path, .. } => async_path.is_some(),
            Backend::Partition { .. } => can_submit(dev.parent),
        },
        None => false,
    }
}

fn kick(handle: usize) {
    if let Some(dev) = device(handle) {
        match dev.backend {
            Backend::Driver { submit: Some((_, driver_kick)), ctx, .. } => {
                driver_kick(ctx as *mut u8)
            }
            Backend::Driver { .. } => {}
            Backend::Partition { .. } => kick(dev.parent),
        }
    }
}

fn submit(handle: usize, io: &BlockIo, kick_now: i32) -> i32 {
    let dev = match device(handle) {
        Some(dev) => dev,
        None => return SUBMIT_INVALID,
    };

    let start = match dev.backend {
        Backend::Driver { submit: Some((driver_submit, _)), ctx, .. } => {
            return driver_submit(ctx as *mut u8, io, kick_now);
        }
        Backend::Driver { .. } => return SUBMIT_UNSUPPORTED,
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
        if kick_now != 0 {
            kick(dev.parent);
        }
        return SUBMIT_INVALID;
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

    add(Device {
        name: unsafe { CStr::from_ptr(ops.name.cast()) }.into(),
        capacity: ops.capacity,
        sector_size: ops.sector_size,
        parent: ops.parent,
        backend: Backend::Driver {
            read_sectors,
            write_sectors,
            flush: ops.flush,
            submit,
            ctx: ops.ctx as usize,
        },
    })
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
    read(handle, sector, buf, count)
}

/// Synchronous write, count in sectors: 0 once the device has the data.
///
/// # Safety
/// `buf` holds `count` sectors, and the device's driver may DMA out of it.
#[no_mangle]
pub unsafe extern "C" fn kernel_blockdev_write(
    handle: usize, sector: u64, buf: *const u8, count: u32, fua: i32,
) -> i32 {
    write(handle, sector, buf, count, fua)
}

#[no_mangle]
pub extern "C" fn kernel_blockdev_flush(handle: usize) -> i32 {
    flush(handle)
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
    handle: usize, io: *const BlockIo, kick: i32,
) -> i32 {
    match unsafe { io.as_ref() } {
        Some(io) => submit(handle, io, kick),
        None => SUBMIT_INVALID,
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
