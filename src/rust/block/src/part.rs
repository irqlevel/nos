//! A partition as a block device of its own: everything asked of it is
//! rebased onto the disk it lives on, and refused past its end.
//!
//! The disk is held as a `kcore::block::Disk` -- a handle, not a pointer --
//! and a partition is registered for as long as the kernel runs, so there is
//! nothing to release and no lifetime to keep track of.

use alloc::boxed::Box;
use kcore::block::{
    self, BlockIo, Disk, SubmitError, SUBMIT_BUSY, SUBMIT_INVALID, SUBMIT_OK, SUBMIT_UNSUPPORTED,
};

/// Room for a disk's name and two digits of partition number, NUL included.
pub const NAME_MAX: usize = 16;

pub struct Partition {
    /// The disk it is on
    disk: Disk,
    /// Its first sector on that disk
    start: u64,
    /// Its size, in sectors
    count: u64,
    sector_size: usize,
    /// NUL-terminated: the kernel's device table points into this, so it has
    /// to live as long as the registration, which is for good.
    name: [u8; NAME_MAX],
}

impl Partition {
    /// Whether [sector, sector + count) is inside the partition. The arithmetic
    /// is done the way the C++ it replaces did it -- by subtraction from the
    /// size rather than by adding to the offset -- so nothing can wrap.
    fn within(&self, sector: u64, count: u64) -> bool {
        sector <= self.count && count <= self.count - sector
    }
}

/// Register one partition of `disk` as a device named `name` (without its
/// NUL). The asynchronous path is offered only if the disk has one: a caller
/// picks its path by `can_submit`, and a partition that claimed one its disk
/// cannot serve would have every submission refused.
pub fn register(disk: Disk, start: u64, count: u64, name: &[u8]) -> bool {
    let sector_size = disk.sector_size() as usize;
    if sector_size == 0 || count == 0 || name.is_empty() || name.len() >= NAME_MAX {
        return false;
    }

    let mut part = Box::new(Partition {
        disk,
        start,
        count,
        sector_size,
        name: [0; NAME_MAX],
    });
    part.name[..name.len()].copy_from_slice(name);

    let async_path = disk.can_submit();
    let raw = Box::into_raw(part);

    let ops = block::BlockDeviceOps {
        name: unsafe { (*raw).name.as_ptr() },
        capacity: count,
        sector_size: sector_size as u64,
        read_sectors: read,
        write_sectors: write,
        flush: Some(flush),
        submit: if async_path { Some(submit) } else { None },
        kick: if async_path { Some(kick) } else { None },
        ctx: raw as *mut u8,
        parent: disk.handle(),
    };

    match block::register(&ops) {
        Some(reg) => {
            /* Registration is for the life of the kernel: the table has no
             * way to give a device back, so the handle is deliberately not
             * dropped and the Partition behind ctx is never freed. */
            core::mem::forget(reg);
            true
        }
        None => {
            unsafe { drop(Box::from_raw(raw)) };
            false
        }
    }
}

/* The ops the kernel's device table calls. `ctx` is the Partition that
 * `register` leaked, and it outlives every call. */

extern "C" fn read(ctx: *mut u8, sector: u64, buf: *mut u8, count: u32) -> i32 {
    let part = unsafe { &*(ctx as *const Partition) };
    if !part.within(sector, count as u64) {
        return -1;
    }

    /* The caller's buffer, which the disk's driver DMAs into: count whole
     * sectors of it, as the block API defines the call. */
    let len = count as usize * part.sector_size;
    let slice = unsafe { core::slice::from_raw_parts_mut(buf, len) };
    match part.disk.read(part.start + sector, slice) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

extern "C" fn write(ctx: *mut u8, sector: u64, buf: *const u8, count: u32, fua: i32) -> i32 {
    let part = unsafe { &*(ctx as *const Partition) };
    if !part.within(sector, count as u64) {
        return -1;
    }

    let len = count as usize * part.sector_size;
    let slice = unsafe { core::slice::from_raw_parts(buf, len) };
    match part.disk.write(part.start + sector, slice, fua != 0) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

extern "C" fn flush(ctx: *mut u8) -> i32 {
    let part = unsafe { &*(ctx as *const Partition) };
    match part.disk.flush() {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

extern "C" fn submit(ctx: *mut u8, io: *const BlockIo, kick_now: i32) -> i32 {
    let part = unsafe { &*(ctx as *const Partition) };
    let io = unsafe { &*io };

    /* A flush is about the device, not about a range of it: passed on as it
     * is, like every other call this forwards. */
    if io.op == block::IO_FLUSH {
        return code(unsafe { part.disk.submit(io, kick_now != 0) });
    }

    if !part.within(io.sector, io.count as u64) {
        /* Refused, but a kick is still a kick: what was queued before it
         * without a doorbell is owed one. */
        if kick_now != 0 {
            part.disk.kick();
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
        sector: part.start + io.sector,
        phys: io.phys,
        done: io.done,
        ctx: io.ctx,
    };
    code(unsafe { part.disk.submit(&on_disk, kick_now != 0) })
}

extern "C" fn kick(ctx: *mut u8) {
    let part = unsafe { &*(ctx as *const Partition) };
    part.disk.kick();
}

fn code(result: core::result::Result<(), SubmitError>) -> i32 {
    match result {
        Ok(()) => SUBMIT_OK,
        Err(SubmitError::Busy) => SUBMIT_BUSY,
        Err(SubmitError::Invalid) => SUBMIT_INVALID,
        Err(SubmitError::Unsupported) => SUBMIT_UNSUPPORTED,
    }
}
