#pragma once

namespace Kernel
{

/* Mount the boot-time filesystems as the command line asks (root=, ro,
   fstest=; see docs/filesystems.md). Runs once the disks that appear late
   -- NVMe, above all -- have been probed for partitions.

   The policy itself is Rust (src/rust/fs/src/rootfs.rs), where the
   filesystems it mounts are. */
void MountRootFs();

}
