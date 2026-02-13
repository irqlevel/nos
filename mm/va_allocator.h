#pragma once

#include <include/const.h>
#include <kernel/spin_lock.h>

namespace Kernel
{

namespace Mm
{

class VaAllocator
{
public:
    VaAllocator();
    ~VaAllocator();

    bool Setup(ulong vaStart, ulong vaEnd, ulong blockSize);

    ulong Alloc();
    void Free(ulong va);
    bool Contains(ulong va);

private:
    VaAllocator(const VaAllocator& other) = delete;
    VaAllocator(VaAllocator&& other) = delete;
    VaAllocator& operator=(const VaAllocator& other) = delete;
    VaAllocator& operator=(VaAllocator&& other) = delete;

    u8* BitmapPtr;
    ulong BitmapSize;
    SpinLock Lock;
    ulong VaStart;
    ulong VaEnd;
    ulong BlockSize;
    ulong BlockCount;
};

}
}
