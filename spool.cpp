#include "spool.h"
#include "lock.h"
#include "stdlib.h"
#include "const.h"
#include "panic.h"
#include "trace.h"

namespace Kernel
{

namespace Core
{

SPool::SPool()
    : Usage(0)
    , Size(0)
{
    BlockList.Init();
    PageList.Init();
}

SPool::~SPool()
{
    BugOn(Usage != 0);
    Setup(0);
}

void SPool::Setup(size_t size, PageAllocator* pageAllocator)
{
    Trace(SPoolLL, "0x%p setup size 0x%p", this, size);

    Shared::AutoLock lock(Lock);

    while (!BlockList.IsEmpty())
    {
        BlockList.RemoveHead();
    }

    while (!PageList.IsEmpty())
    {
        PageAllocer->Free(PageList.RemoveHead());
    }

    Usage = 0;
    Size = size;
    PageAllocer = pageAllocator;
}

bool SPool::CheckSize(size_t size)
{
    if (size == 0 || size >= Shared::PageSize)
        return false;

    return true;
}

void* SPool::Alloc()
{
    Trace(SPoolLL, "0x%p alloc block size 0x%p", this, Size);

    Shared::AutoLock lock(Lock);

    if (!CheckSize(Size))
    {   
        return nullptr;
    }

    if (BlockList.IsEmpty())
    {
        Page* page = static_cast<Page*>(PageAllocer->Alloc());
        if (page == nullptr)
        {
            return nullptr;
        }

        PageList.InsertTail(&page->Link);

        ListEntry* block = reinterpret_cast<ListEntry*>(&page->Data[0]);
        while (Shared::MemAdd(block, Size) <= Shared::MemAdd(page, Shared::PageSize))
        {
            BlockList.InsertTail(block);
            block = static_cast<ListEntry*>(Shared::MemAdd(block, Size));
        }
    }

    Usage++;
    void* block = BlockList.RemoveHead();
    Trace(SPoolLL, "0x%p alloc block %p", this, block);

    return block;
}

void SPool::Free(void* ptr)
{
    Shared::AutoLock lock(Lock);

    Trace(SPoolLL, "Free block %p", ptr);

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