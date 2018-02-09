#include "pool.h"

#include <include/const.h>
#include <kernel/panic.h>
#include <kernel/trace.h>

#include <lib/lock.h>
#include <lib/stdlib.h>

namespace Kernel
{

namespace Mm
{

Pool::Pool()
    : Usage(0)
    , Size(0)
{
    BlockList.Init();
    PageList.Init();
}

Pool::~Pool()
{
    Trace(0, "0x%p usage %u size %u", this, Usage, Size);
    Setup(0);
}

void Pool::Setup(size_t size, class PageAllocator* pageAllocator)
{
    Trace(PoolLL, "0x%p setup size 0x%p", this, size);

    Stdlib::AutoLock lock(Lock);

    while (!BlockList.IsEmpty())
    {
        BlockList.RemoveHead();
    }

    while (!PageList.IsEmpty())
    {
        PageAllocator->Free(PageList.RemoveHead());
    }

    Usage = 0;
    Size = size;
    PageAllocator = pageAllocator;
}

bool Pool::CheckSize(size_t size)
{
    if (size == 0 || size >= Const::PageSize)
        return false;

    return true;
}

void* Pool::Alloc()
{
    Trace(PoolLL, "0x%p alloc block size 0x%p", this, Size);

    Stdlib::AutoLock lock(Lock);

    if (!CheckSize(Size))
    {   
        return nullptr;
    }

    if (BlockList.IsEmpty())
    {
        Page* page = static_cast<Page*>(PageAllocator->Alloc(1));
        if (page == nullptr)
        {
            return nullptr;
        }

        PageList.InsertTail(&page->Link);

        ListEntry* block = reinterpret_cast<ListEntry*>(&page->Data[0]);
        while (Stdlib::MemAdd(block, Size) <= Stdlib::MemAdd(page, Const::PageSize))
        {
            BlockList.InsertTail(block);
            block = static_cast<ListEntry*>(Stdlib::MemAdd(block, Size));
        }
    }

    Usage++;
    void* block = BlockList.RemoveHead();
    Trace(PoolLL, "0x%p alloc block %p", this, block);

    return block;
}

void Pool::Free(void* ptr)
{
    Stdlib::AutoLock lock(Lock);

    Trace(PoolLL, "Free block %p", ptr);

    if (ptr == nullptr)
    {
        Panic("ptr is null");
        return;
    }

    if (!CheckSize(Size))
    {
        Panic("Invalid size");
        return;
    }

    ListEntry* block = static_cast<ListEntry*>(ptr);
    BlockList.InsertTail(block);
    Usage--;
}

}
}