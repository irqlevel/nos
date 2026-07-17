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

    /* True if the physical page containing phyAddr lies in a usable RAM
       (e820 type 1) region. Used to pick cacheability for mappings:
       usable RAM is mapped write-back, everything else (MMIO, reserved,
       ACPI) is mapped uncached. */
    bool IsUsableRam(ulong phyAddr);

    /* True if [phyAddr, phyAddr+len) overlaps any reserved (non-type-1)
       region — e.g. the DTB carve-out on arm64. The free-page scan must
       skip such pages even when a usable-RAM region covers them. */
    bool IsReserved(ulong phyAddr, ulong len);

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