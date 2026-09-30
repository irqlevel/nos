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

/* Regions the kernel adds to the memory map itself once the firmware's are
   in: PageArray's, with room to spare */
static const ulong KernelCarveOuts = 4;

static bool FramebufferPresent;
static u8 FramebufferType;
static FramebufferInfo Framebuffer;

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

const FramebufferInfo* GetFramebufferInfo()
{
    return FramebufferPresent ? &Framebuffer : nullptr;
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

    const void* mbInfoEnd = Stdlib::MemAdd(MbInfo, MbInfo->TotalSize);
    MultiBootTag * tag;
    /* A tag's header is read only once it is known to be inside the info: an
       info with no end tag, or one shorter than its header, ends here */
    for (tag = reinterpret_cast<MultiBootTag*>(MbInfo + 1);
        Stdlib::MemAdd(tag, sizeof(*tag)) <= mbInfoEnd &&
        tag->Type != MultiBootTagTypeEnd;
        )
    {
        /* A malformed tag must not walk past the info buffer or spin forever */
        if (Stdlib::MemAdd(tag, tag->Size) > mbInfoEnd ||
            tag->Size < sizeof(*tag))
        {
            Trace(0, "Malformed tag %lu size %lu, stop parsing", (ulong)tag->Type, (ulong)tag->Size);
            break;
        }

        Trace(0, "Tag %lu Size %lu", (ulong)tag->Type, (ulong)tag->Size);
        switch (tag->Type)
        {
        case MultiBootTagTypeBootDev:
        {
            if (tag->Size < sizeof(MultiBootTagBootDev))
                break;

            MultiBootTagBootDev* bdev = reinterpret_cast<MultiBootTagBootDev*>(tag);
            Trace(0, "Boot dev 0x%lX 0x%lX 0x%lX",
                (ulong)bdev->BiosDev, (ulong)bdev->Slice, (ulong)bdev->Part);
            break;
        }
        case MultiBootTagTypeMmap:
        {
            if (tag->Size < sizeof(MultiBootTagMmap))
                break;

            MultiBootTagMmap* mmap = reinterpret_cast<MultiBootTagMmap*>(tag);
            MultiBootMmapEntry* entry;

            if (mmap->EntrySize < sizeof(MultiBootMmapEntry))
                break;

            /* The map is a table of fixed size, and this runs before any
               console a headless machine has: a firmware's map too long for
               it must not stop the boot without a word. So reserved regions
               go in first and usable RAM after, leaving room for the
               kernel's own carve-outs (PageArray's): a reserved region left
               out would be pages handed out that are not RAM, a usable one
               only RAM not used, which is said. Only reserved regions too
               many for the table by themselves stop the boot. */
            auto& memoryMap = Kernel::Mm::MemoryMap::GetInstance();
            ulong dropped = 0;
            ulong droppedBytes = 0;
            for (ulong pass = 0; pass < 2; pass++)
            {
                const bool usablePass = (pass == 1);
                for (entry = &mmap->Entry[0];
                     Stdlib::MemAdd(entry, mmap->EntrySize) <= Stdlib::MemAdd(mmap, mmap->Size);
                     entry = reinterpret_cast<MultiBootMmapEntry*>(Stdlib::MemAdd(entry, mmap->EntrySize)))
                {
                    const bool usable = (entry->Type == MultiBootMemoryAvailable);
                    if (usable != usablePass)
                        continue;

                    Trace(0, "Mmap addr 0x%lX len 0x%lX type %lu",
                        (ulong)entry->Addr, (ulong)entry->Len, (ulong)entry->Type);

                    if (usable && memoryMap.GetRegionCount() + KernelCarveOuts >= Kernel::Mm::MemoryMap::MaxRegions)
                    {
                        dropped++;
                        droppedBytes += entry->Len;
                        continue;
                    }

                    if (!memoryMap.AddRegion((ulong)entry->Addr, (ulong)entry->Len, (ulong)entry->Type))
                        Panic("Can't add memory region");
                }
            }

            if (dropped != 0)
                Trace(0, "mm: %lu usable regions, %lu MiB, past what the memory map holds, not used",
                    dropped, droppedBytes / Const::MB);
            break;
        }
        case MultiBootTagTypeCmdline:
        {
            if (tag->Size <= sizeof(MultiBootTagString))
                break;

            MultiBootTagString* cmdLine = reinterpret_cast<MultiBootTagString*>(tag);

            /* The string must be NUL-terminated within the tag */
            size_t maxLen = tag->Size - sizeof(MultiBootTagString);
            bool terminated = false;
            for (size_t i = 0; i < maxLen; i++)
            {
                if (cmdLine->String[i] == '\0')
                {
                    terminated = true;
                    break;
                }
            }
            if (!terminated)
            {
                Trace(0, "Cmdline tag is not NUL-terminated, ignored");
                break;
            }

            Trace(0, "Cmdline %s", cmdLine->String);
            Kernel::Parameters::GetInstance().Parse(cmdLine->String);

            break;
        }
        case MultiBootTagTypeAcpiOld:
        {
            SaveAcpiRsdp(tag, false);
            Trace(0, "Acpi old RSDP tag, size %lu", (ulong)tag->Size);
            break;
        }
        case MultiBootTagTypeAcpiNew:
        {
            SaveAcpiRsdp(tag, true);
            Trace(0, "Acpi new RSDP tag, size %lu", (ulong)tag->Size);
            break;
        }
        case MultiBootTagTypeFramebuffer:
        {
            if (tag->Size < MultiBootTagFramebufferCommonSize)
                break;

            MultiBootTagFramebuffer* fb = reinterpret_cast<MultiBootTagFramebuffer*>(tag);
            FramebufferPresent = true;
            FramebufferType = fb->FbType;

            Framebuffer.Addr = fb->Addr;
            Framebuffer.Pitch = fb->Pitch;
            Framebuffer.Width = fb->Width;
            Framebuffer.Height = fb->Height;
            Framebuffer.Bpp = fb->Bpp;
            Framebuffer.Type = fb->FbType;

            /* Color field layout only exists in the RGB variant */
            if (fb->FbType == MultiBootFramebufferTypeRgb &&
                tag->Size >= sizeof(MultiBootTagFramebuffer))
            {
                Framebuffer.RedPos = fb->RedFieldPosition;
                Framebuffer.RedSize = fb->RedMaskSize;
                Framebuffer.GreenPos = fb->GreenFieldPosition;
                Framebuffer.GreenSize = fb->GreenMaskSize;
                Framebuffer.BluePos = fb->BlueFieldPosition;
                Framebuffer.BlueSize = fb->BlueMaskSize;
            }
            else if (fb->FbType == MultiBootFramebufferTypeRgb)
            {
                /* Truncated tag: assume the usual little-endian xRGB8888 */
                Framebuffer.RedPos = 16;
                Framebuffer.RedSize = 8;
                Framebuffer.GreenPos = 8;
                Framebuffer.GreenSize = 8;
                Framebuffer.BluePos = 0;
                Framebuffer.BlueSize = 8;
            }

            Trace(0, "Framebuffer addr 0x%lX %lux%lu pitch %lu bpp %lu type %lu",
                (ulong)fb->Addr, (ulong)fb->Width, (ulong)fb->Height,
                (ulong)fb->Pitch, (ulong)fb->Bpp, (ulong)fb->FbType);
            break;
        }
        default:
        {
            break;
        }
        }

        /* Size >= sizeof(*tag) was checked above, so this always advances */
        tag = reinterpret_cast<MultiBootTag*>(Stdlib::MemAdd(tag, (tag->Size + 7) & ~7));
    }
}

}

}