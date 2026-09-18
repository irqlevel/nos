#include "nanofs.h"

#include <lib/stdlib.h>

extern "C" {

int rust_nanofs_mount(const char* path, unsigned long pathLen, unsigned long device, int readOnly);
int rust_nanofs_format(unsigned long device);

}

namespace Kernel
{

int NanoFsMount(const char* path, ulong device, bool readOnly)
{
    if (path == nullptr || device == 0)
        return NanoFsNotMounted;

    return rust_nanofs_mount(path, Stdlib::StrLen(path), device, readOnly ? 1 : 0);
}

bool NanoFsFormat(ulong device)
{
    if (device == 0)
        return false;

    return rust_nanofs_format(device) == 0;
}

}
