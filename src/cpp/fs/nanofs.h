#pragma once

namespace Kernel
{

class BlockDevice;

/* nanofs itself is Rust (src/rust/fs/src/nanofs.rs): 1024 inodes, 16384
   data blocks of 4 KiB, a CRC32 on every block and on every file's data,
   and copy-on-write writes with the inode committed last. What is left here
   is the way in. */

/* What NanoFsMount answers */
enum NanoFsMounted
{
    NanoFsMountedRw = 0,
    NanoFsMountedRo = 1,
    NanoFsNotMounted = -1,
};

/* Mount dev's nanofs at path. The filesystem belongs to the VFS from here
   on: an unmount releases it. */
int NanoFsMount(const char* path, BlockDevice* dev, bool readOnly = false);

/* Write a fresh nanofs onto dev, with an empty root directory. */
bool NanoFsFormat(BlockDevice* dev);

}
