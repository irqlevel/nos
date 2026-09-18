//! What gets mounted at boot, and where.
//!
//! `root=` names the filesystem that becomes `/`: a device, a label, a UUID,
//! or `auto`, which takes the ext2 whose volume label is `nos` -- what
//! scripts/mkrootfs.sh and scripts/build-disk.sh put on theirs. A device that
//! carries no ext2 is tried as nanofs.
//!
//! With no root on disk there is a fallback layout instead, the one the ISO
//! had before there was a root to boot from: a ramfs on `/`, the first ext2
//! found read-only on `/boot`, and the first nanofs found read-write on
//! `/data`. A procfs goes on `/proc` either way.

use kcore::block::{self, Disk};
use kcore::procinfo::{self, Root};
use kcore::trace;

use crate::ext2;
use crate::nanofs;
use crate::procfs;
use crate::ramfs;

/// `root=auto` takes the ext2 carrying this label.
const LABEL_AUTO: &[u8] = b"nos";

/// Big enough to cross the direct and the single-indirect block range at any
/// block size, and the double-indirect one at 1 KiB blocks, which is what the
/// smoke-test image uses; small enough not to notice at boot.
const FSTEST_SIZE: usize = 300 * 1024;

/// The device `root=` names, or None.
fn find_root(mode: &Root, value: &[u8], uuid: &[u8; 16]) -> Option<Disk> {
    if let Root::Device = mode {
        let name = core::str::from_utf8(value).ok()?;
        return Disk::open(name);
    }

    for index in 0..block::count() {
        let dev = match block::at(index) {
            Some(dev) => dev,
            None => continue,
        };

        let mut id = ext2::Identity { uuid: [0; 16], label: [0; 17] };
        if !ext2::probe(&dev, &mut id) {
            continue;
        }

        let label = &id.label[..id.label.iter().position(|b| *b == 0).unwrap_or(16)];
        let matches = match mode {
            Root::Auto => label == LABEL_AUTO,
            Root::Label => label == value,
            Root::Uuid => &id.uuid == uuid,
            _ => false,
        };
        if matches {
            return Some(dev);
        }
    }

    None
}

/// ext2 if the device carries one, else nanofs; false when neither mounts.
fn mount_root_on(dev: &Disk, read_only: bool) -> bool {
    let mut name = [0u8; 32];
    let mut id = ext2::Identity { uuid: [0; 16], label: [0; 17] };

    if ext2::probe(dev, &mut id) {
        let mounted = ext2::mount_at("/", dev.handle(), read_only);
        if mounted >= 0 {
            trace!(0, "MountRootFs: mounted ext2 on / from {} ({})",
                dev.name(&mut name).unwrap_or("?"),
                if mounted == 1 { "ro" } else { "rw" });
            return true;
        }
        trace!(0, "MountRootFs: mounting ext2 from {} failed",
            dev.name(&mut name).unwrap_or("?"));
        return false;
    }

    let mounted = nanofs::mount_at("/", dev.handle(), read_only);
    if mounted >= 0 {
        trace!(0, "MountRootFs: mounted nanofs on / from {} ({})",
            dev.name(&mut name).unwrap_or("?"),
            if read_only { "ro" } else { "rw" });
        return true;
    }

    trace!(0, "MountRootFs: {} carries no filesystem this kernel mounts",
        dev.name(&mut name).unwrap_or("?"));
    false
}

/// No root on disk: a ramfs on /, the first ext2 found read-only on /boot,
/// the first nanofs found read-write on /data.
fn mount_fallback_layout() {
    let vfs = match crate::vfs_instance() {
        Some(vfs) => vfs,
        None => return,
    };

    if !ramfs::mount_at("/", false) {
        trace!(0, "MountRootFs: failed to mount ramfs on /");
        return;
    }
    trace!(0, "MountRootFs: mounted ramfs on / (rw)");

    vfs.create(b"/boot", true);

    let mut name = [0u8; 32];
    for index in 0..block::count() {
        let dev = match block::at(index) {
            Some(dev) => dev,
            None => continue,
        };
        let mut id = ext2::Identity { uuid: [0; 16], label: [0; 17] };
        if !ext2::probe(&dev, &mut id) {
            continue;
        }
        if ext2::mount_at("/boot", dev.handle(), true) >= 0 {
            trace!(0, "MountRootFs: mounted ext2 on /boot from {} (ro)",
                dev.name(&mut name).unwrap_or("?"));
            break;
        }
    }

    vfs.create(b"/data", true);

    for index in 0..block::count() {
        let dev = match block::at(index) {
            Some(dev) => dev,
            None => continue,
        };
        let mut id = ext2::Identity { uuid: [0; 16], label: [0; 17] };
        if ext2::probe(&dev, &mut id) {
            continue;
        }
        if nanofs::mount_at("/data", dev.handle(), false) >= 0 {
            trace!(0, "MountRootFs: mounted nanofs on /data from {} (rw)",
                dev.name(&mut name).unwrap_or("?"));
            break;
        }
    }
}

fn mount_procfs() {
    let vfs = match crate::vfs_instance() {
        Some(vfs) => vfs,
        None => return,
    };

    if vfs.stat(b"/proc").is_none() && !vfs.create(b"/proc", true) {
        trace!(0, "MountRootFs: no /proc directory and cannot make one");
        return;
    }

    if procfs::mount_at("/proc") {
        trace!(0, "MountRootFs: mounted procfs on /proc (ro)");
    }
}

/// Mount what the kernel boots with. Called once, from C++, after the block
/// devices are registered.
#[no_mangle]
pub extern "C" fn rust_mount_root_fs() {
    let mut value = [0u8; 48];
    let mut uuid = [0u8; 16];
    let (mode, len) = procinfo::root_spec(&mut value, &mut uuid);
    if let Root::None = mode {
        return;
    }

    let value = &value[..len];
    let mut mounted = false;
    match find_root(&mode, value, &uuid) {
        Some(dev) => mounted = mount_root_on(&dev, procinfo::root_read_only()),
        None => {
            if !matches!(mode, Root::Auto) {
                trace!(0, "MountRootFs: root {} not found",
                    core::str::from_utf8(value).unwrap_or("?"));
            }
        }
    }

    if !mounted {
        mount_fallback_layout();
    }

    mount_procfs();

    if procinfo::root_fstest() {
        if crate::selftest::run("/", FSTEST_SIZE, None) {
            trace!(0, "fstest: passed");
        } else {
            trace!(0, "fstest: FAILED");
        }
    }
}
