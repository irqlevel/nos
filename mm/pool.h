#pragma once


#include "page_allocator.h"

#include <kernel/spin_lock.h>
#include <lib/list_entry.h>

namespace Kernel
{

namespace Mm
{

class Pool
{
public:
    Pool();
    virtual ~Pool();

    void Setup(size_t size, PageAllocator* pageAllocator = nullptr);
    void* Alloc();
    void Free(void *ptr);

private:
    using ListEntry = Stdlib::ListEntry;

    bool CheckSize(size_t size);

    struct Page {
        ListEntry Link;
        u8 Data[1]; 
    };

    ulong Usage;
    size_t Size;
    ListEntry PageList;
    ListEntry BlockList;
    SpinLock Lock;
    PageAllocator* PageAllocator;
};

}
}