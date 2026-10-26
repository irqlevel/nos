#pragma once


#include "page_allocator.h"

#include <kernel/atomic.h>
#include <kernel/spin_lock.h>
#include <lib/list_entry.h>

namespace kernel
{

namespace mm
{

class Pool
{
public:
    Pool();
    virtual ~Pool();

    void init(size_t block_size, PageAllocator* pg_alloc);
    void* alloc(ulong tag);
    void free(void *ptr);

    /* Give back the empty pages the pool keeps; whether there was one.
       Called without a spin lock held, as Free's page release is. */
    bool trim();

    /* For the per-CPU caches in front of the pools (AllocatorImpl): up to
       count free blocks off their pages, and free blocks back onto them,
       each under one hold of Lock. take_free takes no new page -- none free
       is 0 -- and put_free gives back any page that empties past the one the
       pool keeps, after letting go of Lock. A block between the two is
       nobody's and still carries FREED_TAG. */
    size_t take_free(void** blocks, size_t count);
    void put_free(void* const* blocks, size_t count);
    static constexpr size_t PUT_FREE_MAX = 16;

    /* A free block handed to an owner, and an owner's block back to free:
       a compare-and-swap of its tag each, so that a block given to two
       owners, or freed twice -- on two CPUs at once included -- is caught
       where it happens. No lock: the block is the caller's alone. */
    static void claim(void* ptr, ulong tag);
    static void mark_freed(void* ptr);

private:

    using ListEntry = stdlib::ListEntry;

    struct Page {
        ListEntry link;
        ListEntry block_list;
        ulong max_block_count;
        ulong block_count;
        u8 data[consts::PAGE_SIZE - 2 * sizeof(ListEntry) - 2 * sizeof(ulong)];
    };

    static_assert(sizeof(Page) == consts::PAGE_SIZE, "invalid size");

    /* Link threads a free block on its page's block_list and is unused while
       the block is out. Tag is FREED_TAG from the page's making until an
       owner has the block, and the owner's tag until it is freed: what the
       report of blocks nobody freed reads, page by page. */
    struct Block {
        ListEntry link;
        Atomic tag;
    };

    /* Tag stamped on freed blocks to detect double-free */
    static const ulong FREED_TAG = 0xF7EEF7EEF7EEF7EEUL;

    /* How many pages with every block free the pool keeps on free_page_list_
       rather than give back. One is what stops an allocation and its free
       from costing a page each when they are all the pool has -- every one
       of the largest size's, whose page holds a single block. */
    static constexpr ulong EMPTY_PAGES_KEPT = 1;

    /* A page from the page allocator, its blocks threaded on its list;
       nullptr if there is none. Called without Lock. */
    Page* new_page();

    /* The index-th block of a page */
    Block* block_at(Page* page, ulong index);

    /* A block off the first page on free_page_list_, which is not empty, its
       tag still FREED_TAG. Lock must be held. */
    Block* take_locked();

    /* A free block back onto its page. Returns the page if that left it one
       empty page more than the pool keeps, for the caller to give back once
       Lock is let go of; nullptr otherwise. Lock must be held. */
    Page* put_locked(Block* block);

    size_t block_size_;
    ListEntry free_page_list_;
    ListEntry page_list_;
    SpinLock lock_;
    PageAllocator* pg_alloc_;
    /* Blocks off their pages: their owners', and those the per-CPU caches
       hold */
    ulong block_count_;
    ulong peek_block_count_;
    /* Pages on free_page_list_ with every block free */
    ulong empty_pages_;
};

}
}
