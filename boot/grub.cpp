#include "grub.h"

#include <kernel/trace.h>
#include <kernel/parameters.h>
#include <mm/memory_map.h>
#include <lib/stdlib.h>

namespace Kernel
{

namespace Grub
{

void ParseMultiBootInfo(MultiBootInfoHeader *MbInfo)
{
    Trace(0, "MbInfo %p", MbInfo);

    MultiBootTag * tag;
    for (tag = reinterpret_cast<MultiBootTag*>(MbInfo + 1);
        tag->Type != MultiBootTagTypeEnd;
        tag = reinterpret_cast<MultiBootTag*>(Shared::MemAdd(tag, (tag->Size + 7) & ~7)))
    {
        Trace(0, "Tag %u Size %u", (ulong)tag->Type, (ulong)tag->Size);
        switch (tag->Type)
        {
        case MultiBootTagTypeBootDev:
        {
            MultiBootTagBootDev* bdev = reinterpret_cast<MultiBootTagBootDev*>(tag);
            Trace(0, "Boot dev 0x%p 0x%p 0x%p",
                (ulong)bdev->BiosDev, (ulong)bdev->Slice, (ulong)bdev->Part);
            break;
        }
        case MultiBootTagTypeMmap:
        {
            MultiBootTagMmap* mmap = reinterpret_cast<MultiBootTagMmap*>(tag);
            MultiBootMmapEntry* entry;

            for (entry = &mmap->Entry[0];
                 Shared::MemAdd(entry, mmap->EntrySize) <= Shared::MemAdd(mmap, mmap->Size);
                 entry = reinterpret_cast<MultiBootMmapEntry*>(Shared::MemAdd(entry, mmap->EntrySize)))
            {
                Trace(0, "Mmap addr 0x%p len 0x%p type %u",
                    entry->Addr, entry->Len, (ulong)entry->Type);

                if (!Kernel::MemoryMap::GetInstance().AddRegion(entry->Addr, entry->Len, entry->Type))
                    Panic("Can't add memory region");

            }
            break;
        }
        case MultiBootTagTypeCmdline:
        {
            MultiBootTagString* cmdLine = reinterpret_cast<MultiBootTagString*>(tag);

            Trace(0, "Cmdline %s", cmdLine->String);
            if (!Kernel::Parameters::GetInstance().Parse(cmdLine->String)) {
                Trace(0, "Can't parse command line");
                Panic("Can't parse command line");
            }

            break;
        }
        default:
        {
            break;
        }
        }
    }
}

}

}