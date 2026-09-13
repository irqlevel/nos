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

    for (auto link = BlockList.Flink; link != &BlockList; link = link->Flink) {
        auto block = CONTAINING_RECORD(link, Block, Link);
        Trace(0, "0x%p block 0x%p tag 0x%p", this, block, block->Tag);
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

void *Pool::Alloc(ulong tag)
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

        Block* block = reinterpret_cast<Block*>(&page->Data[0]);
        while (Stdlib::MemAdd(block, sizeof(*block) + BlockSize) <= Stdlib::MemAdd(page, Const::PageSize))
        {
            page->BlockList.InsertTail(&block->Link);
            block = static_cast<Block*>(Stdlib::MemAdd(block, sizeof(*block) + BlockSize));
            page->BlockCount++;
        }
        page->MaxBlockCount = page->BlockCount;
        FreePageList.InsertHead(&page->Link);
    }

    Page* page = CONTAINING_RECORD(FreePageList.Flink, Page, Link);
    BugOn(page->BlockList.IsEmpty());
    Block* block = CONTAINING_RECORD(page->BlockList.RemoveHead(), Block, Link);
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

    block->Tag = tag;
    BlockList.InsertHead(&block->Link);
    return block + 1;
}

void Pool::Free(void* ptr)
{
    BugOn(!ptr);
    Page* freePage = nullptr;

    {
        Stdlib::AutoLock lock(Lock);

        BlockCount--;
        Page* page = (Page*)((ulong)ptr & ~(Const::PageSize - 1));
        BugOn((ulong)page & (Const::PageSize - 1));

        Block* block = static_cast<Block*>(ptr) - 1;
        BugOn(block->Tag == FreedTag);
        block->Tag = FreedTag;
        block->Link.RemoveInit();
        BugOn(page->BlockCount >= page->MaxBlockCount);

        page->BlockList.InsertTail(&block->Link);
        page->BlockCount++;
        page->Link.RemoveInit();
        if (page->BlockCount == page->MaxBlockCount)
        {
            freePage = page;
        }
        else
        {
            FreePageList.InsertTail(&page->Link);
        }
    }

    if (freePage)
        PgAlloc->Free(freePage);
}

}
}