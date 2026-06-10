#include "grub.h"

#include <kernel/trace.h>
#include <kernel/parameters.h>
#include <mm/memory_map.h>
#include <lib/stdlib.h>

namespace Kernel
{

namespace Grub
{

/* The multiboot info lives in usable RAM that is later handed to the
   page allocator, so the RSDP is copied out during parsing. RSDP v2 is
   36 bytes; keep some slack. */
static u8 AcpiRsdpCopy[64];
static size_t AcpiRsdpSize;
static bool AcpiRsdpIsNew;

static bool FramebufferPresent;
static u8 FramebufferType;

const void* GetAcpiRsdp(size_t& size)
{
    size = AcpiRsdpSize;
    return (AcpiRsdpSize != 0) ? AcpiRsdpCopy : nullptr;
}

bool HasFramebufferInfo()
{
    return FramebufferPresent;
}

bool IsFramebufferEgaText()
{
    return FramebufferType == MultiBootFramebufferTypeEgaText;
}

static void SaveAcpiRsdp(MultiBootTag* tag, bool isNew)
{
    /* Prefer the v2 (new) RSDP if both tags are present */
    if (AcpiRsdpSize != 0 && AcpiRsdpIsNew && !isNew)
        return;

    if (tag->Size <= sizeof(MultiBootTag))
        return;

    size_t rsdpSize = tag->Size - sizeof(MultiBootTag);
    if (rsdpSize > sizeof(AcpiRsdpCopy))
        rsdpSize = sizeof(AcpiRsdpCopy);

    MultiBootTagAcpi* acpiTag = reinterpret_cast<MultiBootTagAcpi*>(tag);
    Stdlib::MemCpy(AcpiRsdpCopy, acpiTag->Rsdp, rsdpSize);
    AcpiRsdpSize = rsdpSize;
    AcpiRsdpIsNew = isNew;
}

void ParseMultiBootInfo(MultiBootInfoHeader *MbInfo)
{
    Trace(0, "MbInfo %p", MbInfo);

    MultiBootTag * tag;
    for (tag = reinterpret_cast<MultiBootTag*>(MbInfo + 1);
        tag->Type != MultiBootTagTypeEnd;
        tag = reinterpret_cast<MultiBootTag*>(Stdlib::MemAdd(tag, (tag->Size + 7) & ~7)))
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
                 Stdlib::MemAdd(entry, mmap->EntrySize) <= Stdlib::MemAdd(mmap, mmap->Size);
                 entry = reinterpret_cast<MultiBootMmapEntry*>(Stdlib::MemAdd(entry, mmap->EntrySize)))
            {
                Trace(0, "Mmap addr 0x%p len 0x%p type %u",
                    entry->Addr, entry->Len, (ulong)entry->Type);

                if (!Kernel::Mm::MemoryMap::GetInstance().AddRegion((ulong)entry->Addr, (ulong)entry->Len, (ulong)entry->Type))
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
        case MultiBootTagTypeAcpiOld:
        {
            SaveAcpiRsdp(tag, false);
            Trace(0, "Acpi old RSDP tag, size %u", (ulong)tag->Size);
            break;
        }
        case MultiBootTagTypeAcpiNew:
        {
            SaveAcpiRsdp(tag, true);
            Trace(0, "Acpi new RSDP tag, size %u", (ulong)tag->Size);
            break;
        }
        case MultiBootTagTypeFramebuffer:
        {
            MultiBootTagFramebuffer* fb = reinterpret_cast<MultiBootTagFramebuffer*>(tag);
            FramebufferPresent = true;
            FramebufferType = fb->FbType;
            Trace(0, "Framebuffer addr 0x%p %ux%u bpp %u type %u",
                fb->Addr, (ulong)fb->Width, (ulong)fb->Height,
                (ulong)fb->Bpp, (ulong)fb->FbType);
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