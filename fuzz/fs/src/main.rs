//! The storage layers, fuzzed on the host.
//!
//! What is on a disk is whoever wrote it's to choose -- a disk that came
//! with the machine, an image somebody made, a filesystem another kernel
//! left half written -- and the device itself says how big it is and how
//! big a sector is. So a panic, an overflow, a lock broken, a loop that
//! never ends, a write where nothing may write, anywhere on the paths that
//! read a disk, is one somebody else can cause. This program is the
//! kernel's own crates -- `block` (the device table, the partition tables,
//! the claims, the disk log) and `fs` (the VFS, ext2, nanofs, ramfs,
//! procfs, the file ABI, the shell's commands), over `kcore` and `ffi` as
//! they are -- linked with the rest of a kernel written for the purpose
//! (`machine`: the fuzzers' common one, and disks whose media are the
//! fuzzer's, each with a volatile write cache, a power switch and requests
//! that fail).
//!
//! Each target turns random bytes into disks -- images made from the
//! format (`image`: ext2 and nanofs made and checked the way mke2fs and
//! e2fsck do it, partition tables, the disk log's area), and then damaged
//! -- and into what the kernel is asked to do with them, and holds what
//! comes of it to what the format and the layer's own documentation say:
//! the tree a run of operations should have left (`model`), a filesystem
//! e2fsck passes after a clean unmount and one no power cut leaves
//! corrupt, a partition table read as the probe documents it, two writers
//! never on the same sectors.
#![allow(dead_code)]

extern crate alloc;

#[macro_use]
#[path = "../../common/mod.rs"]
mod common;
mod image;
mod machine;
mod model;
mod targets;

pub use common::input::{self, Input};
pub use common::runner::{reached, Target};

#[global_allocator]
static HEAP: machine::heap::Heap = machine::heap::Heap;

fn main() {
    common::runner::main(&common::runner::Fuzzer {
        name: "fs-fuzz",
        targets: targets::ALL,
        boot: machine::boot,
        stats_env: "FS_FUZZ_STATS",
    });
}
