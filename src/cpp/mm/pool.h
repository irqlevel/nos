#pragma once


#include "page_allocator.h"

#include <kernel/atomic.h>
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

    /* Give back the empty pages the pool keeps; whether there was one.
       Called without a spin lock held, as Free's page release is. */
    bool Trim();

    /* For the per-CPU caches in front of the pools (AllocatorImpl): up to
       count free blocks off their pages, and free blocks back onto them,
       each under one hold of Lock. TakeFree takes no new page -- none free
       is 0 -- and PutFree gives back any page that empties past the one the
       pool keeps, after letting go of Lock. A block between the two is
       nobody's and still carries FreedTag. */
    size_t TakeFree(void** blocks, size_t count);
    void PutFree(void* const* blocks, size_t count);
    static constexpr size_t PutFreeMax = 16;

    /* A free block handed to an owner, and an owner's block back to free:
       a compare-and-swap of its tag each, so that a block given to two
       owners, or freed twice -- on two CPUs at once included -- is caught
       where it happens. No lock: the block is the caller's alone. */
    static void Claim(void* ptr, ulong tag);
    static void MarkFreed(void* ptr);

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

    /* Link threads a free block on its page's BlockList and is unused while
       the block is out. Tag is FreedTag from the page's making until an
       owner has the block, and the owner's tag until it is freed: what the
       report of blocks nobody freed reads, page by page. */
    struct Block {
        ListEntry Link;
        Atomic Tag;
    };

    /* Tag stamped on freed blocks to detect double-free */
    static const ulong FreedTag = 0xF7EEF7EEF7EEF7EEUL;

    /* How many pages with every block free the pool keeps on FreePageList
       rather than give back. One is what stops an allocation and its free
       from costing a page each when they are all the pool has -- every one
       of the largest size's, whose page holds a single block. */
    static constexpr ulong EmptyPagesKept = 1;

    /* A page from the page allocator, its blocks threaded on its list;
       nullptr if there is none. Called without Lock. */
    Page* NewPage();

    /* The index-th block of a page */
    Block* BlockAt(Page* page, ulong index);

    /* A block off the first page on FreePageList, which is not empty, its
       tag still FreedTag. Lock must be held. */
    Block* TakeLocked();

    /* A free block back onto its page. Returns the page if that left it one
       empty page more than the pool keeps, for the caller to give back once
       Lock is let go of; nullptr otherwise. Lock must be held. */
    Page* PutLocked(Block* block);

    size_t BlockSize;
    ListEntry FreePageList;
    ListEntry PageList;
    SpinLock Lock;
    PageAllocator* PgAlloc;
    /* Blocks off their pages: their owners', and those the per-CPU caches
       hold */
    ulong BlockCount;
    ulong PeekBlockCount;
    /* Pages on FreePageList with every block free */
    ulong EmptyPages;
};

}
}
