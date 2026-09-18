#pragma once

#include <include/types.h>

namespace Kernel
{

class BlockDevice;

/* ext2 itself is Rust (src/rust/fs/src/ext2.rs): the superblock, the group
   descriptors, the inodes, the directories and every write's commit order.
   What is left here is the way in -- what a device carries, and mounting it
   -- for the C++ that picks a root filesystem and runs the shell. */

/* What a probe reads off an unmounted ext2 superblock: enough to pick a root
   filesystem by label or UUID. crate::ext2::Identity is the same struct. */
struct Ext2Identity
{
    u8 Uuid[16];
    char Label[17];
};

/* Does dev carry an ext2 this kernel would mount? Fills id when it does. */
bool Ext2Probe(BlockDevice* dev, Ext2Identity& id);

/* What Ext2Mount answers */
enum Ext2Mounted
{
    Ext2MountedRw = 0,
    Ext2MountedRo = 1,   /* asked for, or all the image allows */
    Ext2NotMounted = -1,
};

/* Mount dev's ext2 at path. The filesystem belongs to the VFS from here on:
   an unmount releases it. */
int Ext2Mount(const char* path, BlockDevice* dev, bool readOnly);

}
