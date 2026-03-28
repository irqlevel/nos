#pragma once

#include <include/types.h>
#include <boot/grub.h>

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