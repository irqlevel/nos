#pragma once

#include <include/types.h>

namespace Kernel
{

/* ext2 itself is Rust (src/rust/fs/src/ext2.rs): the superblock, the group
   descriptors, the inodes, the directories and every write's commit order.
   What is left here is the way in -- what a device carries, and mounting it
   -- for the C++ that picks a root filesystem and runs the shell. */

/* What Ext2Mount answers */
enum Ext2Mounted
{
    Ext2MountedRw = 0,
    Ext2MountedRo = 1,   /* asked for, or all the image allows */
    Ext2NotMounted = -1,
};

/* Mount the device's ext2 at path. The filesystem belongs to the VFS from
   here on: an unmount releases it. */
int Ext2Mount(const char* path, ulong device, bool readOnly);

}
