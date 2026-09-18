#include "ext2.h"

#include <block/block_device.h>
#include <lib/stdlib.h>

extern "C" {

int rust_ext2_probe(unsigned long device, Kernel::Ext2Identity* id);
int rust_ext2_mount(const char* path, unsigned long pathLen, unsigned long device, int readOnly);

}

namespace Kernel
{

bool Ext2Probe(BlockDevice* dev, Ext2Identity& id)
{
    if (dev == nullptr)
        return false;

    return rust_ext2_probe(dev->GetHandle(), &id) == 0;
}

int Ext2Mount(const char* path, BlockDevice* dev, bool readOnly)
{
    if (path == nullptr || dev == nullptr)
        return Ext2NotMounted;

    return rust_ext2_mount(path, Stdlib::StrLen(path), dev->GetHandle(), readOnly ? 1 : 0);
}

}
