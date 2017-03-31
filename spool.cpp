#include "spool.h"
#include "lock.h"
#include "stdlib.h"
#include "const.h"
#include "panic.h"

namespace Kernel
{

namespace Core
{

SPool::SPool()
    : Usage(0)
{
    BlockList.Init();
    PageList.Init();
}

SPool::~SPool()
{
    BugOn(Usage != 0);
    Reset(0);
}

void SPool::Reset(size_t size, PageAllocator* pageAllocator)
{
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
    if (size == 0 || size >= PAGE_SIZE)
        return false;

    return true;
}

void* SPool::Alloc()
{
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
        while (Shared::MemAdd(block, Size) <= Shared::MemAdd(page, PAGE_SIZE))
        {
            BlockList.InsertTail(block);
            block = static_cast<ListEntry*>(Shared::MemAdd(block, Size));
        }
    }

    Usage++;
    return BlockList.RemoveHead();
}

void SPool::Free(void* ptr)
{
    Shared::AutoLock lock(Lock);

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