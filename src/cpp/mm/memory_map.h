#pragma once

#include <include/types.h>

namespace Kernel
{

namespace Mm
{

class MemoryMap final
{
public:
    static MemoryMap& GetInstance()
    {
        static MemoryMap Instance;

        return Instance;
    }
    ~MemoryMap();

    bool AddRegion(ulong addr, ulong len, ulong type);

    bool FindRegion(ulong base, ulong limit, ulong& start, ulong& end);

    ulong GetKernelStart();

    ulong GetKernelEnd();

    size_t GetRegionCount();

    bool GetRegion(size_t index, ulong& addr, ulong& len, ulong& type);

    /* Usable (type 1) RAM the firmware reported: all of it, the end of the
       highest such region, and how much of it lies at or above limit. The
       kernel can only free-list what the bootstrap linear map reaches, so
       the last one is the RAM the machine has and does not use. */
    ulong GetUsableRamBytes();
    ulong GetUsableRamEnd();
    ulong GetUsableRamBytesAbove(ulong limit);

    /* True if any usable RAM falls in [start, end). The bootstrap map grows
       in 1GiB blocks; a block with no RAM in it is left unmapped rather than
       mapped cacheable over unbacked bus space. */
    bool HasUsableRamIn(ulong start, ulong end);

    /* The two queries a page scan wants instead of IsReserved per page. A
       real server's map has twenty-odd reserved regions and sixteen million
       pages, and asking "is this one reserved" about each of them is thirty
       million interval tests for a handful of boundaries.

       GetReservedEnd: if the page at phyAddr overlaps a reserved region, the
       page-aligned end of the reserved run it is in; 0 if it does not.
       GetNextReservedStart: the page-aligned start of the next reserved
       region above phyAddr, or limit if none is below limit. Between the two
       a scan moves boundary to boundary. */
    ulong GetReservedEnd(ulong phyAddr);
    ulong GetNextReservedStart(ulong phyAddr, ulong limit);

    /* Name of a firmware region type, for the shell. */
    static const char* GetRegionTypeName(ulong type);

    /* True if the physical page containing phyAddr lies in a usable RAM
       (e820 type 1) region. Used to pick cacheability for mappings:
       usable RAM is mapped write-back, everything else (MMIO, reserved,
       ACPI) is mapped uncached. */
    bool IsUsableRam(ulong phyAddr);

    /* True if [phyAddr, phyAddr+len) overlaps any reserved (non-type-1)
       region — e.g. the DTB carve-out on arm64. The free-page scan must
       skip such pages even when a usable-RAM region covers them. */
    bool IsReserved(ulong phyAddr, ulong len);

    /* e820/EFI region types. Ordinary usable RAM is 1; everything else is
       MMIO, firmware-reserved or ACPI, and the page allocator stays out of
       it. The kernel adds regions of its own (the arm64 DTB, PageArray's
       backing) as reserved, which is what keeps GetFreePages off them. */
    static const ulong UsableRamType = 1;
    static const ulong ReservedType = 2;

    static const ulong KernelSpaceBase = 0xFFFF800000000000;

    static const ulong UserSpaceMax = 0x00007FFFFFFFFFFF;

private:
    MemoryMap(const MemoryMap& other) = delete;
    MemoryMap(MemoryMap&& other) = delete;
    MemoryMap& operator=(const MemoryMap& other) = delete;
    MemoryMap& operator=(MemoryMap&& other) = delete;

    MemoryMap();


    struct MemoryRegion
    {
        ulong Addr;
        ulong Len;
        ulong Type;
    };

    MemoryRegion Region[64];
    size_t Size;
};

}
}