#pragma once

namespace Kernel
{

/* procfs itself is Rust (src/rust/fs/src/procfs.rs): /proc/version,
   /proc/cmdline and /proc/interrupts, the last written again on every
   lookup. What is left here is the way in. */

/* Mount procfs at path, read-only. The filesystem belongs to the VFS from
   here on: an unmount releases it. */
bool ProcFsMount(const char* path);

}
