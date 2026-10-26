#include "pool.h"

#include <include/const.h>
#include <kernel/panic.h>
#include <kernel/trace.h>

#include <lib/lock.h>
#include <lib/stdlib.h>
#include <mm/new.h>

namespace kernel
{

namespace mm
{

Pool::Pool()
    : block_size_(0)
    , pg_alloc_(nullptr)
    , block_count_(0)
    , peek_block_count_(0)
    , empty_pages_(0)
{
}

Pool::~Pool()
{
    stdlib::AutoLock lock(lock_);

    trace(0, "0x%p block_size %lu peekBlockCount %lu", this, block_size_, peek_block_count_);

    if (block_count_ != 0)
        trace(0, "0x%p block_size %lu blockCount %lu", this, block_size_, block_count_);

    /* The blocks nobody freed, page by page: the pool keeps no list of the
       blocks out, which every allocation would have to take Lock to join */
    ListEntry* lists[] = { &page_list_, &free_page_list_ };
    for (ListEntry* list : lists)
    {
        for (auto link = list->flink; link != list; link = link->flink)
        {
            auto page = CONTAINING_RECORD(link, Page, link);
            for (ulong i = 0; i < page->max_block_count; i++)
            {
                Block* block = block_at(page, i);
                ulong tag = static_cast<ulong>(block->tag.get());
                if (tag != FREED_TAG)
                    trace(0, "0x%p block 0x%p tag 0x%lX", this, block, tag);
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
       Watchdog::unregister_spin_lock under Cpu::~Cpu, Serial::~Serial or
       whichever static drew the neighbouring slot. The leaks are reported
       above; their memory is not worth anything at halt. */
}

void Pool::init(size_t block_size, class PageAllocator* pg_alloc)
{
    BUG_ON(block_count_);
    BUG_ON(block_size_);
    BUG_ON(pg_alloc_ != nullptr);
    BUG_ON(!page_list_.is_empty());
    BUG_ON(!free_page_list_.is_empty());
    BUG_ON(peek_block_count_);

    block_size_ = block_size;
    pg_alloc_ = pg_alloc;
}

Pool::Page* Pool::new_page()
{
    Page* page = static_cast<Page*>(pg_alloc_->alloc(1, true));
    if (page == nullptr)
        return nullptr;

    page->link.init();
    page->block_count = 0;
    page->block_list.init();

    Block* block = block_at(page, 0);
    while (stdlib::mem_add(block, sizeof(*block) + block_size_) <= stdlib::mem_add(page, consts::PAGE_SIZE))
    {
        /* Made here, the page being raw memory: Tag is an Atomic, whose
           lifetime a constructor begins */
        block = new (block) Block;
        block->tag.set(static_cast<long>(FREED_TAG));
        page->block_list.insert_tail(&block->link);
        page->block_count++;
        block = block_at(page, page->block_count);
    }
    page->max_block_count = page->block_count;
    return page;
}

Pool::Block* Pool::block_at(Page* page, ulong index)
{
    return static_cast<Block*>(stdlib::mem_add(&page->data[0], index * (sizeof(Block) + block_size_)));
}

/* A page for the pool is allocated with the lock let go of, as Free gives
   one back: the page allocator may shoot down every CPU's TLB and wait for
   them all, and a CPU spinning on this lock with its interrupts off would
   never answer. The new page is this call's alone until it is on the list;
   if another CPU put one there meanwhile, it goes back. */
void *Pool::alloc(ulong tag)
{
    Block* block = nullptr;
    {
        stdlib::AutoLock lock(lock_);

        if (!free_page_list_.is_empty())
            block = take_locked();
    }

    if (block == nullptr)
    {
        Page* page = new_page();
        if (page == nullptr)
            return nullptr;

        Page* spare = nullptr;
        {
            stdlib::AutoLock lock(lock_);

            if (free_page_list_.is_empty())
            {
                free_page_list_.insert_head(&page->link);
                empty_pages_++;
            }
            else
                spare = page;
            block = take_locked();
        }

        if (spare != nullptr)
            pg_alloc_->free(spare);
    }

    claim(block + 1, tag);
    return block + 1;
}

Pool::Block* Pool::take_locked()
{
    Page* page = CONTAINING_RECORD(free_page_list_.flink, Page, link);
    BUG_ON(page->block_list.is_empty());
    if (page->block_count == page->max_block_count)
    {
        BUG_ON(empty_pages_ == 0);
        empty_pages_--;
    }
    Block* block = CONTAINING_RECORD(page->block_list.remove_head(), Block, link);
    block->link.init();
    page->block_count--;
    if (page->block_list.is_empty())
    {
        BUG_ON(page->block_count);
        page->link.remove_init();
        page_list_.insert_tail(&page->link);
    }

    block_count_++;
    if (block_count_ > peek_block_count_)
        peek_block_count_ = block_count_;

    /* Free since its page was made or its owner let it go: a tag of
       anything else is a write into a block nobody owned */
    BUG_ON(static_cast<ulong>(block->tag.get()) != FREED_TAG);
    return block;
}

Pool::Page* Pool::put_locked(Block* block)
{
    BUG_ON(static_cast<ulong>(block->tag.get()) != FREED_TAG);
    Page* page = reinterpret_cast<Page*>(reinterpret_cast<ulong>(block) & ~(consts::PAGE_SIZE - 1));
    BUG_ON(page->block_count >= page->max_block_count);

    block_count_--;
    page->block_list.insert_tail(&block->link);
    page->block_count++;
    page->link.remove_init();
    if (page->block_count == page->max_block_count && empty_pages_ >= EMPTY_PAGES_KEPT)
        return page;

    /* At the tail: the pages before it, with blocks in use, are the ones to
       fill first */
    if (page->block_count == page->max_block_count)
        empty_pages_++;
    free_page_list_.insert_tail(&page->link);
    return nullptr;
}

void Pool::free(void* ptr)
{
    BUG_ON(!ptr);
    mark_freed(ptr);

    Page* free_page;
    {
        stdlib::AutoLock lock(lock_);
        free_page = put_locked(static_cast<Block*>(ptr) - 1);
    }

    if (free_page)
        pg_alloc_->free(free_page);
}

size_t Pool::take_free(void** blocks, size_t count)
{
    stdlib::AutoLock lock(lock_);

    size_t taken = 0;
    while (taken < count && !free_page_list_.is_empty())
        blocks[taken++] = take_locked() + 1;
    return taken;
}

void Pool::put_free(void* const* blocks, size_t count)
{
    BUG_ON(count > PUT_FREE_MAX);

    /* At most one page a block: a page another block of the batch would go
       back to cannot also be empty */
    Page* release[PUT_FREE_MAX];
    size_t released = 0;
    {
        stdlib::AutoLock lock(lock_);

        for (size_t i = 0; i < count; i++)
        {
            Page* page = put_locked(static_cast<Block*>(blocks[i]) - 1);
            if (page != nullptr)
                release[released++] = page;
        }
    }

    for (size_t i = 0; i < released; i++)
        pg_alloc_->free(release[i]);
}

void Pool::claim(void* ptr, ulong tag)
{
    Block* block = static_cast<Block*>(ptr) - 1;
    long was = block->tag.cmpxchg(static_cast<long>(tag), static_cast<long>(FREED_TAG));
    /* Not free: handed out twice, or written into while free */
    BUG_ON(static_cast<ulong>(was) != FREED_TAG);
}

void Pool::mark_freed(void* ptr)
{
    Block* block = static_cast<Block*>(ptr) - 1;
    for (;;)
    {
        long was = block->tag.get();
        /* Freed already: a double free */
        BUG_ON(static_cast<ulong>(was) == FREED_TAG);
        if (block->tag.cmpxchg(static_cast<long>(FREED_TAG), was) == was)
            return;
    }
}

bool Pool::trim()
{
    ListEntry empty;
    {
        stdlib::AutoLock lock(lock_);

        ListEntry* link = free_page_list_.flink;
        while (link != &free_page_list_)
        {
            ListEntry* next = link->flink;
            Page* page = CONTAINING_RECORD(link, Page, link);
            if (page->block_count == page->max_block_count)
            {
                page->link.remove_init();
                empty.insert_tail(&page->link);
                BUG_ON(empty_pages_ == 0);
                empty_pages_--;
            }
            link = next;
        }
        BUG_ON(empty_pages_ != 0);
    }

    bool trimmed = false;
    while (!empty.is_empty())
    {
        pg_alloc_->free(CONTAINING_RECORD(empty.remove_head(), Page, link));
        trimmed = true;
    }
    return trimmed;
}

}
}