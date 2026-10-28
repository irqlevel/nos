//! Which of the two ext drivers a device's filesystem is for. ext2's --
//! kept as it is, for the roots made before ext4 -- takes what it always
//! took but a filesystem with a journal, which it would write past: rev 1
//! with no incompatible feature but filetype. ext4's takes the rest of the
//! family, and mounts what it can of it (`extfmt::features::classify`).

use block::Disk;
use extfmt::features::{compat, incompat};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Driver {
    Ext2,
    Ext4,
}

/// The driver for `dev`'s filesystem; None for a device with no superblock
/// of the family on it.
pub fn driver_for(dev: &Disk) -> Option<Driver> {
    let sb = crate::ext4::probe_super(dev)?;
    let plain = sb.rev_level() >= 1 && sb.incompat() & !incompat::FILETYPE == 0
        && sb.compat() & compat::HAS_JOURNAL == 0;
    Some(if plain { Driver::Ext2 } else { Driver::Ext4 })
}

/// Mount `dev`'s filesystem at `path` with the driver for it: what that
/// driver's `mount_at` answers -- 0 read-write, 1 read-only, -1 not
/// mounted -- and the driver's name.
pub fn mount_at(path: &str, dev: &Disk, read_only: bool) -> (i32, &'static str) {
    match driver_for(dev) {
        Some(Driver::Ext2) => (crate::ext2::mount_at(path, dev.handle(), read_only), "ext2"),
        Some(Driver::Ext4) => (crate::ext4::mount_at(path, dev.handle(), read_only), "ext4"),
        None => (-1, "ext2"),
    }
}
