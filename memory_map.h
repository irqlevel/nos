#pragma once

#include "grub.h"
#include "types.h"

namespace Kernel
{

namespace Core
{

class MemoryMap final
{
public:
    static MemoryMap& GetInstance(Grub::MultiBootInfo *MbInfo)
    {
        static MemoryMap instance(MbInfo);

        return instance;
    }
    ~MemoryMap();

    bool GetFreeRegion(ulong base, ulong& start, ulong& end);

private:
    MemoryMap(Grub::MultiBootInfo *MbInfo);

    MemoryMap() = delete;
    MemoryMap(const MemoryMap& other) = delete;
    MemoryMap(MemoryMap&& other) = delete;
    MemoryMap& operator=(const MemoryMap& other) = delete;
    MemoryMap& operator=(MemoryMap&& other) = delete;

    Grub::MultiBootInfo *MbInfoPtr;
};

}

}