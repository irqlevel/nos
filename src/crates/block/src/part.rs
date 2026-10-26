//! A partition as a block device of its own: everything asked of it is
//! rebased onto the disk it lives on, and refused past its end.
//!
//! It is the device table that does that (`table::register_partition`): a
//! partition is an entry there that says which device it is a stretch of and
//! from where, so its I/O is the disk's a few sectors on, without leaving the
//! table -- no ops of its own, and nothing allocated to hang them on.

use core::ffi::CStr;

use crate::disk::Disk;

use crate::table;

/// Room for a disk's name and two digits of partition number, NUL included.
pub const NAME_MAX: usize = 16;

/// Register one partition of `disk` as a device named `name` (without its
/// NUL).
pub fn register(disk: Disk, start: u64, count: u64, name: &[u8]) -> bool {
    if name.is_empty() || name.len() >= NAME_MAX {
        return false;
    }

    let mut terminated = [0u8; NAME_MAX];
    terminated[..name.len()].copy_from_slice(name);

    match CStr::from_bytes_until_nul(&terminated) {
        Ok(name) => table::register_partition(disk.handle(), start, count, name),
        Err(_) => false,
    }
}
