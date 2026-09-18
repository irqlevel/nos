#include "procfs.h"

#include <lib/stdlib.h>

extern "C" {

int rust_procfs_mount(const char* path, unsigned long pathLen);

}

namespace Kernel
{

bool ProcFsMount(const char* path)
{
    if (path == nullptr)
        return false;

    return rust_procfs_mount(path, Stdlib::StrLen(path)) == 0;
}

}
