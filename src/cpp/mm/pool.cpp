#include "pool.h"

#include <include/const.h>
#include <kernel/panic.h>
#include <kernel/trace.h>

#include <lib/lock.h>
#include <lib/stdlib.h>
#include <mm/new.h>

namespace Kernel
{

namespace Mm
{

Pool::Pool()
    : BlockSize(0)
    , PgAlloc(nullptr)
    , BlockCount(0)
    , PeekBlockCount(0)
    , EmptyPages(0)
{
}

Pool::~Pool()
{
    Stdlib::AutoLock lock(Lock);

    Trace(0, "0x%p blockSize %lu peekBlockCount %lu", this, BlockSize, PeekBlockCount);

    if (BlockCount != 0)
        Trace(0, "0x%p blockSize %lu blockCount %lu", this, BlockSize, BlockCount);

    /* The blocks nobody freed, page by page: the pool keeps no list of the
       blocks out, which every allocation would have to take Lock to join */
    ListEntry* lists[] = { &PageList, &FreePageList };
    for (ListEntry* list : lists)
    {
        for (auto link = list->Flink; link != list; link = link->Flink)
        {
            auto page = CONTAINING_RECORD(link, Page, Link);
            for (ulong i = 0; i < page->MaxBlockCount; i++)
            {
                Block* block = BlockAt(page, i);
                ulong tag = static_cast<ulong>(block->Tag.Get());
                if (tag != FreedTag)
                    Trace(0, "0x%p block 0x%p tag 0x%lX", this, block, tag);
            }
        }
    }

    /* The pages stay mapped. Free() hands a page back the moment its last
       block goes, so every page still on either list holds a block nobody
       freed: an object that is still there and still linked into whatever
       it was linked into. Releasing them here unmapped live objects under
       everything destroyed after the allocator -- which on x86 is every
       static constructed before the heap: CpuTable, Serial, Dmesg, the
       watchdog. The watchdog's buckets are where that bit: a leaked
       object's lock shares a bucket with a static's lock, and unregistering
       the static one writes into the leaked one's links -- a page fault in
       Watchdog::UnregisterSpinLock under Cpu::~Cpu, Serial::~Serial or
       whichever static drew the neighbouring slot. The leaks are reported
       above; their memory is not worth anything at halt. */
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

Pool::Page* Pool::NewPage()
{
    Page* page = static_cast<Page*>(PgAlloc->Alloc(1, true));
    if (page == nullptr)
        return nullptr;

    page->Link.Init();
    page->BlockCount = 0;
    page->BlockList.Init();

    Block* block = BlockAt(page, 0);
    while (Stdlib::MemAdd(block, sizeof(*block) + BlockSize) <= Stdlib::MemAdd(page, Const::PageSize))
    {
        /* Made here, the page being raw memory: Tag is an Atomic, whose
           lifetime a constructor begins */
        block = new (block) Block;
        block->Tag.Set(static_cast<long>(FreedTag));
        page->BlockList.InsertTail(&block->Link);
        page->BlockCount++;
        block = BlockAt(page, page->BlockCount);
    }
    page->MaxBlockCount = page->BlockCount;
    return page;
}

Pool::Block* Pool::BlockAt(Page* page, ulong index)
{
    return static_cast<Block*>(Stdlib::MemAdd(&page->Data[0], index * (sizeof(Block) + BlockSize)));
}

/* A page for the pool is allocated with the lock let go of, as Free gives
   one back: the page allocator may shoot down every CPU's TLB and wait for
   them all, and a CPU spinning on this lock with its interrupts off would
   never answer. The new page is this call's alone until it is on the list;
   if another CPU put one there meanwhile, it goes back. */
void *Pool::Alloc(ulong tag)
{
    Block* block = nullptr;
    {
        Stdlib::AutoLock lock(Lock);

        if (!FreePageList.IsEmpty())
            block = TakeLocked();
    }

    if (block == nullptr)
    {
        Page* page = NewPage();
        if (page == nullptr)
            return nullptr;

        Page* spare = nullptr;
        {
            Stdlib::AutoLock lock(Lock);

            if (FreePageList.IsEmpty())
            {
                FreePageList.InsertHead(&page->Link);
                EmptyPages++;
            }
            else
                spare = page;
            block = TakeLocked();
        }

        if (spare != nullptr)
            PgAlloc->Free(spare);
    }

    Claim(block + 1, tag);
    return block + 1;
}

Pool::Block* Pool::TakeLocked()
{
    Page* page = CONTAINING_RECORD(FreePageList.Flink, Page, Link);
    BugOn(page->BlockList.IsEmpty());
    if (page->BlockCount == page->MaxBlockCount)
    {
        BugOn(EmptyPages == 0);
        EmptyPages--;
    }
    Block* block = CONTAINING_RECORD(page->BlockList.RemoveHead(), Block, Link);
    block->Link.Init();
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

    /* Free since its page was made or its owner let it go: a tag of
       anything else is a write into a block nobody owned */
    BugOn(static_cast<ulong>(block->Tag.Get()) != FreedTag);
    return block;
}

Pool::Page* Pool::PutLocked(Block* block)
{
    BugOn(static_cast<ulong>(block->Tag.Get()) != FreedTag);
    Page* page = reinterpret_cast<Page*>(reinterpret_cast<ulong>(block) & ~(Const::PageSize - 1));
    BugOn(page->BlockCount >= page->MaxBlockCount);

    BlockCount--;
    page->BlockList.InsertTail(&block->Link);
    page->BlockCount++;
    page->Link.RemoveInit();
    if (page->BlockCount == page->MaxBlockCount && EmptyPages >= EmptyPagesKept)
        return page;

    /* At the tail: the pages before it, with blocks in use, are the ones to
       fill first */
    if (page->BlockCount == page->MaxBlockCount)
        EmptyPages++;
    FreePageList.InsertTail(&page->Link);
    return nullptr;
}

void Pool::Free(void* ptr)
{
    BugOn(!ptr);
    MarkFreed(ptr);

    Page* freePage;
    {
        Stdlib::AutoLock lock(Lock);
        freePage = PutLocked(static_cast<Block*>(ptr) - 1);
    }

    if (freePage)
        PgAlloc->Free(freePage);
}

size_t Pool::TakeFree(void** blocks, size_t count)
{
    Stdlib::AutoLock lock(Lock);

    size_t taken = 0;
    while (taken < count && !FreePageList.IsEmpty())
        blocks[taken++] = TakeLocked() + 1;
    return taken;
}

void Pool::PutFree(void* const* blocks, size_t count)
{
    BugOn(count > PutFreeMax);

    /* At most one page a block: a page another block of the batch would go
       back to cannot also be empty */
    Page* release[PutFreeMax];
    size_t released = 0;
    {
        Stdlib::AutoLock lock(Lock);

        for (size_t i = 0; i < count; i++)
        {
            Page* page = PutLocked(static_cast<Block*>(blocks[i]) - 1);
            if (page != nullptr)
                release[released++] = page;
        }
    }

    for (size_t i = 0; i < released; i++)
        PgAlloc->Free(release[i]);
}

void Pool::Claim(void* ptr, ulong tag)
{
    Block* block = static_cast<Block*>(ptr) - 1;
    long was = block->Tag.Cmpxchg(static_cast<long>(tag), static_cast<long>(FreedTag));
    /* Not free: handed out twice, or written into while free */
    BugOn(static_cast<ulong>(was) != FreedTag);
}

void Pool::MarkFreed(void* ptr)
{
    Block* block = static_cast<Block*>(ptr) - 1;
    for (;;)
    {
        long was = block->Tag.Get();
        /* Freed already: a double free */
        BugOn(static_cast<ulong>(was) == FreedTag);
        if (block->Tag.Cmpxchg(static_cast<long>(FreedTag), was) == was)
            return;
    }
}

bool Pool::Trim()
{
    ListEntry empty;
    {
        Stdlib::AutoLock lock(Lock);

        ListEntry* link = FreePageList.Flink;
        while (link != &FreePageList)
        {
            ListEntry* next = link->Flink;
            Page* page = CONTAINING_RECORD(link, Page, Link);
            if (page->BlockCount == page->MaxBlockCount)
            {
                page->Link.RemoveInit();
                empty.InsertTail(&page->Link);
                BugOn(EmptyPages == 0);
                EmptyPages--;
            }
            link = next;
        }
        BugOn(EmptyPages != 0);
    }

    bool trimmed = false;
    while (!empty.IsEmpty())
    {
        PgAlloc->Free(CONTAINING_RECORD(empty.RemoveHead(), Page, Link));
        trimmed = true;
    }
    return trimmed;
}

}
}