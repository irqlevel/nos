#include "ext2.h"

#include <lib/stdlib.h>

extern "C" {

int rust_ext2_mount(const char* path, unsigned long pathLen, unsigned long device, int readOnly);

}

namespace Kernel
{

int Ext2Mount(const char* path, ulong device, bool readOnly)
{
    if (path == nullptr || device == 0)
        return Ext2NotMounted;

    return rust_ext2_mount(path, Stdlib::StrLen(path), device, readOnly ? 1 : 0);
}

}
