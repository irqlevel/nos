#include "block_device.h"

#include <kernel/trace.h>
#include <lib/stdlib.h>

namespace Kernel
{

BlockDeviceTable::BlockDeviceTable()
    : Count(0)
{
    for (ulong i = 0; i < MaxDevices; i++)
        Devices[i] = nullptr;
}

BlockDeviceTable::~BlockDeviceTable()
{
}

bool BlockDeviceTable::Register(BlockDevice* dev)
{
    if (Count >= MaxDevices || dev == nullptr)
        return false;

    Devices[Count] = dev;
    Count++;

    Trace(0, "BlockDevice registered: %s capacity %u sectors",
        dev->GetName(), dev->GetCapacity());

    return true;
}

BlockDevice* BlockDeviceTable::Find(const char* name)
{
    for (ulong i = 0; i < Count; i++)
    {
        if (Devices[i] && Stdlib::StrCmp(Devices[i]->GetName(), name) == 0)
            return Devices[i];
    }
    return nullptr;
}

void BlockDeviceTable::Dump(Stdlib::Printer& printer)
{
    if (Count == 0)
    {
        printer.Printf("no block devices\n");
        return;
    }

    for (ulong i = 0; i < Count; i++)
    {
        if (!Devices[i])
            continue;

        u64 cap = Devices[i]->GetCapacity();
        u64 secSize = Devices[i]->GetSectorSize();
        u64 mb = (cap * secSize) / (1024 * 1024);

        printer.Printf("%s  %u sectors (%u MB)  %u bytes/sector\n",
            Devices[i]->GetName(), cap, mb, secSize);
    }
}

ulong BlockDeviceTable::GetCount()
{
    return Count;
}

}