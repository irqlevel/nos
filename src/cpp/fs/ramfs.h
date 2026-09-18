#pragma once

namespace Kernel
{

/* ramfs itself is Rust (src/rust/fs/src/ramfs.rs): a file is one growing
   buffer, a directory a list of children, and nothing survives a reboot.
   What is left here is the way in. */

/* Mount a fresh ramfs at path. The filesystem belongs to the VFS from here
   on: an unmount releases it. */
bool RamFsMount(const char* path, bool readOnly = false);

}
