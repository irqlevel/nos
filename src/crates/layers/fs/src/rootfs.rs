//! What gets mounted at boot, and where.
//!
//! `root=` names the filesystem that becomes `/`: a device, a label, a UUID,
//! or `auto`, which takes the ext4 or ext2 whose volume label is `nos` --
//! an ext4 is what nos-install, scripts/mkrootfs.sh and scripts/build-disk.sh
//! make, an ext2 what an install before ext4 made -- by the driver for it
//! (`extfs`). A label or a UUID two devices carry finds neither: one of
//! them may be a guest's.
//!
//! With no root on disk there is a fallback layout instead, the one the ISO
//! had before there was a root to boot from: a ramfs on `/` and the first
//! of the ext family found read-only on `/boot`. A procfs goes on `/proc`
//! either way.

use block::Disk;
use kcore::procinfo::{self, Root};
use kcore::klog;

use crate::ext2;
use crate::extfs;
use crate::procfs;
use crate::ramfs;

/// `root=auto` takes the ext4 or ext2 carrying this label.
const LABEL_AUTO: &[u8] = b"nos";

/// Big enough to cross the direct and the single-indirect block range at any
/// block size, and the double-indirect one at 1 KiB blocks, which is what the
/// smoke-test image uses; small enough not to notice at boot.
const FSTEST_SIZE: usize = 300 * 1024;

/// The device `root=` names, or None -- None too where a label or a UUID is
/// on two devices, said: a guest given a device of nos's (`hv devices`)
/// can put an ext4 labelled `nos` on it, or a copy of nos's own root, and
/// the next boot must not take the guest's for its root because the table
/// happened to list it first.
fn find_root(mode: &Root, value: &[u8], uuid: &[u8; 16]) -> Option<Disk> {
    if let Root::Device = mode {
        let name = core::str::from_utf8(value).ok()?;
        return Disk::open(name);
    }

    let mut found: Option<Disk> = None;
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
        if !matches {
            continue;
        }
        if let Some(first) = &found {
            klog!(0, "MountRootFs: {} and {} both carry the root root= asks for -- neither is taken, as one may be \
                   a guest's or a copy", first.name(), dev.name());
            return None;
        }
        found = Some(dev);
    }

    found
}

/// The ext4 or ext2 the device carries; false when it carries neither, or
/// what it carries does not mount.
fn mount_root_on(dev: &Disk, read_only: bool) -> bool {
    let mut id = ext2::Identity { uuid: [0; 16], label: [0; 17] };

    if ext2::probe(dev, &mut id) {
        let (mounted, driver) = extfs::mount_at("/", dev, read_only);
        if mounted >= 0 {
            klog!(0, "MountRootFs: mounted {} on / from {} ({})", driver,
                dev.name(),
                if mounted == 1 { "ro" } else { "rw" });
            return true;
        }
        klog!(0, "MountRootFs: mounting {} from {} failed", driver,
            dev.name());
        return false;
    }

    klog!(0, "MountRootFs: {} carries no filesystem this kernel mounts",
        dev.name());
    false
}

/// No root on disk: a ramfs on /, and the first of the ext family found
/// read-only on /boot.
fn mount_fallback_layout() {
    let vfs = match crate::vfs_instance() {
        Some(vfs) => vfs,
        None => return,
    };

    if !ramfs::mount_at("/", false) {
        klog!(0, "MountRootFs: failed to mount ramfs on /");
        return;
    }
    klog!(0, "MountRootFs: mounted ramfs on / (rw)");

    vfs.create(b"/boot", true);

    for index in 0..block::count() {
        let dev = match block::at(index) {
            Some(dev) => dev,
            None => continue,
        };
        let mut id = ext2::Identity { uuid: [0; 16], label: [0; 17] };
        if !ext2::probe(&dev, &mut id) {
            continue;
        }
        let (mounted, driver) = extfs::mount_at("/boot", &dev, true);
        if mounted >= 0 {
            klog!(0, "MountRootFs: mounted {} on /boot from {} (ro)", driver,
                dev.name());
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
        klog!(0, "MountRootFs: no /proc directory and cannot make one");
        return;
    }

    if procfs::mount_at("/proc") {
        klog!(0, "MountRootFs: mounted procfs on /proc (ro)");
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
                klog!(0, "MountRootFs: root {} not found",
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
            klog!(0, "fstest: passed");
        } else {
            klog!(0, "fstest: FAILED");
        }
    }
}
