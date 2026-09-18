#include "nanofs.h"

#include <block/block_device.h>
#include <lib/stdlib.h>

extern "C" {

int rust_nanofs_mount(const char* path, unsigned long pathLen, unsigned long device, int readOnly);
int rust_nanofs_format(unsigned long device);

}

namespace Kernel
{

int NanoFsMount(const char* path, BlockDevice* dev, bool readOnly)
{
    if (path == nullptr || dev == nullptr)
        return NanoFsNotMounted;

    return rust_nanofs_mount(path, Stdlib::StrLen(path), dev->GetHandle(), readOnly ? 1 : 0);
}

bool NanoFsFormat(BlockDevice* dev)
{
    if (dev == nullptr)
        return false;

    return rust_nanofs_format(dev->GetHandle()) == 0;
}

}
