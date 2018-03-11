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
    : BlockSize(0)
    , PgAlloc(nullptr)
    , BlockCount(0)
    , PeekBlockCount(0)
{
}

Pool::~Pool()
{
    Stdlib::AutoLock lock(Lock);

    Trace(0, "0x%p blockSize %u peekBlockCount %u", this, BlockSize, PeekBlockCount);

    if (BlockCount != 0)
        Trace(0, "0x%p blockSize %u blockCount %u", this, BlockSize, BlockCount);

    while (!FreePageList.IsEmpty())
    {
        PgAlloc->Free(FreePageList.RemoveHead());
    }

    while (!PageList.IsEmpty())
    {
        PgAlloc->Free(PageList.RemoveHead());
    }
}

void Pool::Init(size_t blockSize, class PageAllocator* pgAlloc)
{
    BugOn(BlockCount);
    BugOn(BlockSize);
    BugOn(PgAlloc != nullptr);
    BugOn(!PageList.IsEmpty());
    BugOn(!FreePageList.IsEmpty());
    BugOn(PeekBlockCount);

    BlockSize = blockSize;
    PgAlloc = pgAlloc;
}

void *Pool::Alloc()
{
    Stdlib::AutoLock lock(Lock);

    if (FreePageList.IsEmpty())
    {
        Page* page = static_cast<Page*>(PgAlloc->Alloc(1));
        if (page == nullptr)
        {
            return nullptr;
        }
        page->BlockCount = 0;
        page->BlockList.Init();

        ListEntry* block = reinterpret_cast<ListEntry*>(&page->Data[0]);
        while (Stdlib::MemAdd(block, BlockSize) <= Stdlib::MemAdd(page, Const::PageSize))
        {
            page->BlockList.InsertTail(block);
            block = static_cast<ListEntry*>(Stdlib::MemAdd(block, BlockSize));
            page->BlockCount++;
        }
        page->MaxBlockCount = page->BlockCount;
        FreePageList.InsertHead(&page->Link);
    }

    Page* page = CONTAINING_RECORD(FreePageList.Flink, Page, Link);
    BugOn(page->BlockList.IsEmpty());
    void* block = page->BlockList.RemoveHead();
    page->BlockCount--;
    if (page->BlockList.IsEmpty())
    {
        BugOn(page->BlockCount);
        page->Link.RemoveInit();
        PageList.InsertTail(&page->Link);
    }

    BlockCount++;
    if (BlockCount > PeekBlockCount)
        PeekBlockCount = BlockCount;

    return block;
}

void Pool::Free(void* ptr)
{
    BugOn(!ptr);
    Stdlib::AutoLock lock(Lock);

    BlockCount--;
    Page* page = (Page*)((ulong)ptr & ~(Const::PageSize - 1));
    BugOn((ulong)page & (Const::PageSize - 1));

    ListEntry* block = static_cast<ListEntry*>(ptr);
    page->BlockList.InsertTail(block);
    page->BlockCount++;
    page->Link.RemoveInit();
    if (page->BlockCount == page->MaxBlockCount)
    {
        PgAlloc->Free(page);
    }
    else
    {
        FreePageList.InsertTail(&page->Link);
    }
}

}
}