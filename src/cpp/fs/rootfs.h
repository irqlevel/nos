#pragma once

namespace Kernel
{

/* Mount the boot-time filesystems as the command line asks (root=, ro,
   fstest=; see docs/filesystems.md). Runs once the disks that appear late
   -- NVMe, above all -- have been probed for partitions. */
void MountRootFs();

}
