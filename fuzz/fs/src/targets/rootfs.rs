//! What the kernel mounts at boot: `root=` -- none, `auto`, a device, a
//! label, a UUID -- over disks that carry an ext2 labelled `nos` or not,
//! another ext2, a nanofs, something that only looks like an ext2, or
//! nothing, some of them on the partitions of a disk; `ro`; `fstest=on`.
//! Held to what `rootfs.rs` documents: the root the spec names, found the
//! way it says -- the first ext2 in the table's order whose label or UUID
//! matches -- and ext2 if it carries one, nanofs if not; with no root, a
//! ramfs at `/`, the first ext2 read-only at `/boot`, the first nanofs at
//! `/data`; procfs at `/proc` either way. And every filesystem it mounted
//! for writing left clean by the unmount of everything that shutdown does.

use crate::image::{ext2 as ext2img, nanofs as nanoimg, part};
use crate::machine::{self, disk};
use crate::model::{Data, Node};
use crate::{reached, Input};

/// What a disk of the input's carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Carries {
    /// An ext2 with this label.
    Ext2 { label: u8 },
    /// An ext2 whose revision the probe does not take: not an ext2 to it.
    OldExt2,
    Nanofs,
    Nothing,
}

const LABELS: [&[u8]; 3] = [b"nos", b"data", b""];

fn geometry(uuid: [u8; 16], label: &[u8]) -> ext2img::Geometry {
    let mut l = [0u8; 16];
    l[..label.len()].copy_from_slice(label);
    ext2img::Geometry {
        log_block_size: 2,
        blocks: 512,
        blocks_per_group: 32768,
        inodes_per_group: 64,
        inode_size: 128,
        sparse_super: true,
        large_file: true,
        dir_index: false,
        ro_compat_extra: 0,
        reserved_blocks: 0,
        label: l,
        uuid,
    }
}

/// The image of what a device carries, and how many bytes the device is.
fn image(c: Carries, uuid: [u8; 16], r: &mut Input) -> (disk::Media, u64) {
    let mut tree = std::collections::BTreeMap::new();
    tree.insert(b"f".to_vec(), Node::File(Data::from(&crate::input::noise(r.u32(), 100))));
    let tree = Node::Dir(tree);
    match c {
        Carries::Ext2 { .. } | Carries::OldExt2 => {
            let label = match c {
                Carries::Ext2 { label } => LABELS[label as usize % 3],
                _ => b"nos",
            };
            let g = geometry(uuid, label);
            let made = ext2img::mkfs(&g, &tree, &Default::default(), 1_749_000_000, 1).expect("a sound geometry");
            let mut m = made.media;
            if c == Carries::OldExt2 {
                m.write(ext2img::SB_OFFSET + ext2img::sb::REV_LEVEL as u64, &0u32.to_le_bytes());
            }
            (m, 512 * 4096)
        }
        Carries::Nanofs => (nanoimg::mkfs(&tree, uuid, 0).expect("a small tree").media, nanoimg::DEVICE_BYTES),
        Carries::Nothing => (disk::Media::from_bytes(&crate::input::noise(r.u32(), 4096)), 1 << 20),
    }
}

/// A device of the table, as the reference sees it.
struct Dev {
    name: String,
    handle: usize,
    carries: Carries,
    uuid: [u8; 16],
}

pub fn rootfs(r: &mut Input) {
    /* The disks: each carrying something, or split by an MBR in two, a
     * partition apiece. */
    let mut devs: Vec<Dev> = Vec::new();
    let mut slots: Vec<(usize, Vec<(Carries, [u8; 16], u64)>)> = Vec::new();
    let pick = |r: &mut Input| -> Carries {
        match r.u8() % 5 {
            0 | 1 => Carries::Ext2 { label: r.below(3) as u8 },
            2 => Carries::Nanofs,
            3 => Carries::OldExt2,
            _ => Carries::Nothing,
        }
    };
    for slot in 0..r.range(1, 3) as usize {
        let name = format!("vd{}", (b'a' + slot as u8) as char);
        let mut uuid = [0u8; 16];
        uuid[0] = slot as u8 + 1;
        uuid[1] = r.u8() % 2;
        if r.chance(64) {
            /* Two partitions. */
            let (a, b) = (pick(r), pick(r));
            let (ma, na) = image(a, uuid, r);
            let mut ub = uuid;
            ub[2] = 0xB;
            let (mb, nb) = image(b, ub, r);
            let (sa, sb) = (na / 512, nb / 512);
            let e1 = part::MbrEntry { status: 0, kind: 0x83, start: 2048, size: sa as u32 };
            let e2 = part::MbrEntry { status: 0, kind: 0x83, start: (2048 + sa) as u32, size: sb as u32 };
            let none = part::MbrEntry::default();
            let mut m = disk::Media::from_bytes(&part::mbr(&[e1, e2, none, none], part::MBR_SIGNATURE));
            ma.copy_into(&mut m, 2048 * 512);
            mb.copy_into(&mut m, (2048 + sa) * 512);
            disk::insert(slot, disk::Disk::new(&name, 512, 2048 + sa + sb, m));
            slots.push((slot, vec![(a, uuid, na), (b, ub, nb)]));
        } else {
            let c = pick(r);
            let (m, n) = image(c, uuid, r);
            disk::insert(slot, disk::Disk::new(&name, 512, n / 512, m));
            slots.push((slot, vec![(c, uuid, n)]));
        }
        let Some(d) = disk::register(slot) else { return };
        let parts = &slots.last().expect("pushed").1;
        devs.push(Dev { name: name.clone(), handle: d.handle(),
                        carries: if parts.len() == 1 { parts[0].0 } else { Carries::Nothing }, uuid });
    }
    block::rust_partitions_probe();
    for (slot, parts) in &slots {
        if parts.len() == 2 {
            for (k, (c, u, _)) in parts.iter().enumerate() {
                let pname = format!("vd{}{}", (b'a' + *slot as u8) as char, k + 1);
                let d = block::Disk::open(&pname).expect("the probe registers the partitions of an MBR");
                devs.push(Dev { name: pname, handle: d.handle(), carries: *c, uuid: *u });
            }
        }
    }
    /* The reference walks the table's order, as the kernel does. */
    devs.sort_by_key(|d| d.handle);

    /* The command line. */
    let mode = r.below(5) as i32;
    let value: Vec<u8> = match mode {
        2 => {
            let n = devs.len();
            if n > 0 && r.u8() % 4 != 0 { devs[r.below(n as u64) as usize].name.clone().into_bytes() } else { b"vdz".to_vec() }
        }
        3 => LABELS[r.below(3) as usize].to_vec(),
        _ => Vec::new(),
    };
    let mut uuid = [0u8; 16];
    uuid[0] = r.range(1, 3) as u8;
    uuid[1] = r.u8() % 2;
    let ro = r.chance(32);
    let mut p = machine::params();
    p.root_mode = mode;
    p.root_value = value.clone();
    p.root_uuid = uuid;
    p.root_ro = ro;
    p.fstest = r.chance(32);
    machine::set_params(p);

    /* What the documentation says is mounted. */
    let is_ext2 = |d: &Dev| matches!(d.carries, Carries::Ext2 { .. });
    let root = match mode {
        0 => None,
        2 => devs.iter().position(|d| d.name.as_bytes() == value),
        _ => devs.iter().position(|d| is_ext2(d) && match (mode, d.carries) {
            (1, Carries::Ext2 { label }) => LABELS[label as usize % 3] == b"nos",
            (3, Carries::Ext2 { label }) => LABELS[label as usize % 3] == value.as_slice(),
            (4, _) => d.uuid == uuid,
            _ => false,
        }),
    };
    let mut want: Vec<(Vec<u8>, &str)> = Vec::new();
    let mut root_mounted = false;
    if mode != 0 {
        if let Some(i) = root {
            match devs[i].carries {
                Carries::Ext2 { .. } => {
                    want.push((b"/".to_vec(), "ext2"));
                    root_mounted = true;
                }
                Carries::Nanofs => {
                    want.push((b"/".to_vec(), "nanofs"));
                    root_mounted = true;
                }
                _ => {}
            }
        }
        if !root_mounted {
            want.push((b"/".to_vec(), "ramfs"));
            if devs.iter().any(is_ext2) {
                want.push((b"/boot".to_vec(), "ext2"));
            }
            /* The first device the probe does not call ext2 that nanofs
             * mounts on -- a disk and its partition overlap, so a disk
             * split in two is never one. */
            if devs.iter().any(|d| d.carries == Carries::Nanofs) {
                want.push((b"/data".to_vec(), "nanofs"));
            }
        }
        want.push((b"/proc".to_vec(), "procfs"));
    }

    fs::rootfs::rust_mount_root_fs();

    let vfs = fs::vfs_instance().expect("the VFS");
    let mut got: Vec<(Vec<u8>, &str)> = (0..vfs.mount_count()).filter_map(|i| vfs.mount_info(i))
        .map(|m| (m.path().to_vec(), m.fs_name)).collect();
    /* A read-only root has no /proc to mount on unless it had one. */
    if ro && root_mounted {
        want.retain(|(p, _)| p != b"/proc");
        got.retain(|(p, _)| p != b"/proc");
    }
    got.sort();
    want.sort();
    invariant!(got == want, "root={} {:?} (ro {}) mounted {:?}, where rootfs.rs says {:?}", mode,
               String::from_utf8_lossy(&value), ro,
               got.iter().map(|(p, n)| format!("{} {}", String::from_utf8_lossy(p), n)).collect::<Vec<_>>(),
               want.iter().map(|(p, n)| format!("{} {}", String::from_utf8_lossy(p), n)).collect::<Vec<_>>());
    reached(match (mode, root_mounted) {
        (0, _) => "no root asked for",
        (_, true) => "a root mounted from disk",
        (_, false) => "the fallback layout",
    });

    /* Shutdown: everything down, and what was mounted for writing clean. */
    vfs.unmount_all();
    for (slot, parts) in &slots {
        let m = disk::with(*slot, |d| d.current.clone());
        let mut at = if parts.len() == 2 { 2048 * 512 } else { 0 };
        for (c, _, n) in parts {
            if let Carries::Ext2 { .. } = c {
                let img = disk::Media::from_bytes(&m.bytes(at, (*n).min(1 << 21) as usize));
                let chk = ext2img::check(&img, *n);
                invariant!(chk.corrupt.is_empty() && chk.unclean.is_empty(), "an ext2 the boot mounted is left {:?} \
                           {:?}", chk.corrupt, chk.unclean);
            }
            at += n;
        }
    }
}
