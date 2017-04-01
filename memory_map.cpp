#include "memory_map.h"
#include "trace.h"
#include "stdlib.h"

namespace Kernel
{

namespace Core
{

const int MmapLL = 3;

MemoryMap::MemoryMap(Grub::MultiBootInfo *MbInfo)
    : MbInfoPtr(MbInfo)
{
    Trace(MmapLL, "Mmap: mbInfo %p flags %p bdev %p cmd %p",
        MbInfoPtr, MbInfoPtr->Flags, MbInfoPtr->BootDevice, MbInfoPtr->CmdLine);

    Grub::MemoryMap* mmap = reinterpret_cast<Grub::MemoryMap*>(MbInfoPtr->MmapAddr);
	while(reinterpret_cast<void*>(mmap) <
        Shared::MemAdd(reinterpret_cast<void*>(MbInfoPtr->MmapAddr), MbInfoPtr->MmapLength))
    {
        Trace(MmapLL, "Mmap: map %p%p %p%p %p",
            mmap->BaseAddrHigh, mmap->BaseAddrLow, mmap->LengthHigh, mmap->LengthLow, mmap->Type);

		mmap = static_cast<Kernel::Grub::MemoryMap*>(
            Shared::MemAdd(mmap, mmap->Size + sizeof(mmap->Size)));
	}

    Grub::Module* module = reinterpret_cast<Grub::Module*>(MbInfoPtr->ModsAddr);
    Trace(MmapLL, "Mmap: modsCount %u", MbInfoPtr->ModsCount);

    while (module < (module + MbInfoPtr->ModsCount))
    {
        Trace(MmapLL, "Mmap: module %p %p %p", module->ModStart, module->ModEnd, module->String);

        module++;
    }
}

bool MemoryMap::GetFreeRegion(ulong base, ulong& start, ulong& end)
{
    Grub::MemoryMap* mmap = reinterpret_cast<Grub::MemoryMap*>(MbInfoPtr->MmapAddr);

    start = end = 0;
	while(reinterpret_cast<void*>(mmap) <
        Shared::MemAdd(reinterpret_cast<void*>(MbInfoPtr->MmapAddr), MbInfoPtr->MmapLength))
    {
        ulong regionBase, regionLength;

        if (mmap->Type != 1)
            goto nextMap;

        if (mmap->BaseAddrHigh != 0 || mmap->LengthHigh != 0)
            goto nextMap;

        if ((mmap->BaseAddrLow + mmap->LengthLow) <= base)
            goto nextMap;

        if (mmap->BaseAddrLow == 0 || mmap->LengthLow == 0)
            goto nextMap;

        regionBase = (mmap->BaseAddrLow < base) ? base : mmap->BaseAddrLow;
        regionLength = (mmap->BaseAddrLow < base) ?
            (mmap->LengthLow - (base - mmap->BaseAddrLow)) : mmap->LengthLow;

        if ((end - start) < regionLength)
        {
            start = regionBase;
            end = regionLength;
        }

nextMap:
		mmap = static_cast<Kernel::Grub::MemoryMap*>(
            Shared::MemAdd(mmap, mmap->Size + sizeof(mmap->Size)));
	}

    if (end > start && start != 0)
        return true;

    return false;
}

MemoryMap::~MemoryMap()
{
}

}

}