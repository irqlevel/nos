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

    void Init(size_t blockSize, PageAllocator* pgAlloc);
    void* Alloc(ulong tag);
    void Free(void *ptr);

private:

    using ListEntry = Stdlib::ListEntry;

    struct Page {
        ListEntry Link;
        ListEntry BlockList;
        ulong MaxBlockCount;
        ulong BlockCount;
        u8 Data[Const::PageSize - 2 * sizeof(ListEntry) - 2 * sizeof(ulong)];
    };

    static_assert(sizeof(Page) == Const::PageSize, "invalid size");

    struct Block {
        ListEntry Link;
        ulong Tag;
    };

    size_t BlockSize;
    ListEntry FreePageList;
    ListEntry PageList;
    ListEntry BlockList;
    SpinLock Lock;
    PageAllocator* PgAlloc;
    ulong BlockCount;
    ulong PeekBlockCount;
};

}
}