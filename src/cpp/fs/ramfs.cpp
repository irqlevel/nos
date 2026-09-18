#include "ramfs.h"

#include <lib/stdlib.h>

extern "C" {

int rust_ramfs_mount(const char* path, unsigned long pathLen, int readOnly);

}

namespace Kernel
{

bool RamFsMount(const char* path, bool readOnly)
{
    if (path == nullptr)
        return false;

    return rust_ramfs_mount(path, Stdlib::StrLen(path), readOnly ? 1 : 0) == 0;
}

}
