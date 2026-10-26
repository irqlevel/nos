#include "page_allocator.h"
#include "page_table.h"

#include <include/const.h>
#include <kernel/panic.h>
#include <kernel/trace.h>
#include <kernel/cpu.h>
#include <lib/list_entry.h>

namespace kernel
{

namespace mm
{

FixedPageAllocator::FixedPageAllocator()
    : page_count_(0)
{
}

FixedPageAllocator::~FixedPageAllocator()
{
    trace(0, "0x%p dtor", this);
}

bool FixedPageAllocator::setup(ulong start, ulong end, ulong page_count)
{
    page_count_ = page_count;
    ulong block_size = page_count * consts::PAGE_SIZE;

    trace(0, "0x%p start 0x%lX end 0x%lX pages %lu", this, start, end, page_count_);
    return va_alloc_.setup(start, end, block_size);
}

void* FixedPageAllocator::alloc()
{
    auto& pt = PageTable::get_instance();
    BUG_ON(page_count_ > MAX_PAGE_COUNT);
    Page* pages[MAX_PAGE_COUNT];

    for (size_t i = 0; i < page_count_; i++)
    {
        pages[i] = pt.alloc_page();
        if (pages[i] == 0)
        {
            for (size_t j = 0; j < i; j++)
                pt.free_page(pages[j]);
            return nullptr;
        }
    }

    ulong va = va_alloc_.alloc();
    if (va == 0)
    {
        for (size_t i = 0; i < page_count_; i++)
            pt.free_page(pages[i]);
        return nullptr;
    }

    if (!pt.map_pages(va, pages, page_count_))
    {
        for (size_t i = 0; i < page_count_; i++)
            pt.free_page(pages[i]);
        /* Remote CPUs may have cached translations from the prefix map_pages
           rolled back (it only invalidates locally); flush everywhere before
           the VA block and the pages become reusable. */
        kernel::CpuTable::get_instance().invalidate_tlb_range(va, page_count_);
        va_alloc_.free(va);
        return nullptr;
    }

    return (void*)va;
}

void* FixedPageAllocator::map(Page* pages)
{
    ulong va = va_alloc_.alloc();
    if (va == 0)
    {
        return nullptr;
    }

    auto& pt = PageTable::get_instance();
    if (!pt.map_contiguous_pages(va, pages, page_count_))
    {
        /* See Alloc: shoot down remote TLBs before reusing the VA. */
        kernel::CpuTable::get_instance().invalidate_tlb_range(va, page_count_);
        va_alloc_.free(va);
        return nullptr;
    }

    return (void*)va;
}

void* FixedPageAllocator::map_phys(ulong* phys_addrs, size_t count)
{
    BUG_ON(count == 0 || count > page_count_);

    ulong va = va_alloc_.alloc();
    if (va == 0)
    {
        return nullptr;
    }

    auto& pt = PageTable::get_instance();
    if (!pt.map_phys_pages(va, phys_addrs, count))
    {
        /* See Alloc: shoot down remote TLBs before reusing the VA. */
        kernel::CpuTable::get_instance().invalidate_tlb_range(va, count);
        va_alloc_.free(va);
        return nullptr;
    }

    return (void*)va;
}

bool FixedPageAllocator::unmap(void* addr, size_t count)
{
    if (!va_alloc_.contains((ulong)addr))
        return false;

    BUG_ON(count == 0 || count > page_count_);

    PageTable::get_instance().unmap_pages((ulong)addr, count, false);

    kernel::CpuTable::get_instance().invalidate_tlb_range((ulong)addr, count);
    va_alloc_.free((ulong)addr);
    return true;
}

bool FixedPageAllocator::free(void* addr)
{
    if (!va_alloc_.contains((ulong)addr))
        return false;

    PageTable::get_instance().unmap_pages((ulong)addr, page_count_, true);

    kernel::CpuTable::get_instance().invalidate_tlb_range((ulong)addr, page_count_);
    va_alloc_.free((ulong)addr);
    return true;
}

bool FixedPageAllocator::contains(void* addr)
{
    return va_alloc_.contains((ulong)addr);
}

PageAllocatorImpl::PageAllocatorImpl()
{
}

bool PageAllocatorImpl::setup()
{
    auto& pt = PageTable::get_instance();
    ulong free_pages_count = pt.get_free_pages_count();
    if (free_pages_count == 0)
        return false;

    ulong start_address = pt.get_va_end();
    ulong end_address = start_address + ((7 * free_pages_count) / 10) * consts::PAGE_SIZE;

    trace(0, "setup 0x%p start 0x%lX end 0x%lX free pages %lu", this, start_address, end_address, free_pages_count);

    size_t size_per_balloc = (end_address - start_address) / stdlib::array_size(fixed_pg_alloc_);
    for (size_t i = 0; i < stdlib::array_size(fixed_pg_alloc_); i++)
    {
        ulong start = start_address + i * size_per_balloc;
        ulong block_size = (1UL << i) * consts::PAGE_SIZE;
        if (!fixed_pg_alloc_[i].setup(stdlib::round_up(start, block_size), start + size_per_balloc, block_size / consts::PAGE_SIZE))
        {
            return false;
        }
    }

    /* The large runs get a window of their own, past the blocks' */
    const ulong large_block = PageTable::MAX_LARGE_MAP_PAGES * consts::PAGE_SIZE;
    const ulong large_start = stdlib::round_up(end_address, large_block);
    if (!large_pg_alloc_.setup(large_start, large_start + LARGE_BLOCK_COUNT * large_block,
            PageTable::MAX_LARGE_MAP_PAGES))
    {
        return false;
    }

    ulong kept_pages = stdlib::min<ulong>(KEPT_PAGES_MAX, free_pages_count / KEPT_RAM_SHARE);
    for (size_t i = 0; i < stdlib::array_size(kept_blocks_); i++)
    {
        stdlib::AutoLock lock(kept_blocks_[i].lock);
        kept_blocks_[i].limit = kept_pages >> i;
    }

    /* A CPU keeps no more of a size than kept_blocks_ may: on a machine too
       small to keep any, none */
    for (auto& cpu : cpu_kept_blocks_)
    {
        stdlib::AutoLock lock(cpu.lock);
        for (size_t i = 0; i < CPU_KEPT_LOGS; i++)
            cpu.sizes[i].limit = stdlib::min<ulong>(CPU_KEPT_PAGES >> i, kept_blocks_[i].limit);
    }

    return true;
}

PageAllocatorImpl::~PageAllocatorImpl()
{
    trace(0, "0x%p dtor", this);
}

void* PageAllocatorImpl::alloc(size_t num_pages, bool zero)
{
    BUG_ON(num_pages == 0);

    size_t log = stdlib::log2(num_pages);
    if (log >= stdlib::array_size(fixed_pg_alloc_))
        return nullptr;

    /* Zeroed when asked, as a block of fresh pages always is: whoever had
       it last wrote into it, and a caller of mm::alloc may count on what
       the page allocator has always handed out */
    void* kept = take_kept(log);
    if (kept != nullptr)
    {
        if (zero)
            stdlib::mem_set(kept, 0, (1UL << log) * consts::PAGE_SIZE);
        return kept;
    }

    void* block = fixed_pg_alloc_[log].alloc();
    /* No page, or no VA of this size: what the caches hold may be both */
    if (block == nullptr && trim())
        block = fixed_pg_alloc_[log].alloc();
    return block;
}

void PageAllocatorImpl::free(void* addr)
{
    size_t log;
    if (!class_of(addr, log))
        PANIC("Can't free addr 0x%p", addr);

    if (!keep(log, addr))
        release(addr);
}

bool PageAllocatorImpl::class_of(void* addr, size_t& log)
{
    for (size_t i = 0; i < stdlib::array_size(fixed_pg_alloc_); i++)
    {
        if (fixed_pg_alloc_[i].contains(addr))
        {
            log = i;
            return true;
        }
    }
    return false;
}

void PageAllocatorImpl::release(void* addr)
{
    size_t log;
    if (!class_of(addr, log) || !fixed_pg_alloc_[log].free(addr))
        PANIC("Can't free addr 0x%p", addr);
}

PageAllocatorImpl::CpuKept* PageAllocatorImpl::this_cpu_kept(size_t log)
{
    ulong cpu;
    if (log >= CPU_KEPT_LOGS || !cache_cpu(cpu))
        return nullptr;
    return &cpu_kept_blocks_[cpu];
}

bool PageAllocatorImpl::keep(size_t log, void* ptr)
{
    BUG_ON(log >= stdlib::array_size(kept_blocks_));
    Kept* block = static_cast<Kept*>(ptr);

    /* A kept block carries the mark, and take_kept clears it on the way
       out: one that has it now is kept already -- a second free -- or its
       owner wrote those very words, so the mark only says where to look */
    if (block->mark == KEPT_MARK && is_kept(log, block))
        PANIC("Double free of 0x%p", ptr);

    CpuKept* cpu = this_cpu_kept(log);
    if (cpu == nullptr)
        return keep_shared(log, block);

    Kept* spill[CPU_KEPT_PAGES];
    ulong spilled = 0;
    bool cached = false;
    {
        stdlib::AutoLock lock(cpu->lock);
        CpuKeptSize& size = cpu->sizes[log];
        if (size.limit != 0)
        {
            if (size.count >= size.limit)
            {
                /* The older half: the newer is likelier still in this
                   CPU's data cache */
                spilled = (size.limit + 1) / 2;
                for (ulong i = 0; i < spilled; i++)
                    spill[i] = size.blocks[i];
                for (ulong i = spilled; i < size.count; i++)
                    size.blocks[i - spilled] = size.blocks[i];
                size.count -= spilled;
            }
            block->mark = KEPT_MARK;
            size.blocks[size.count++] = block;
            cached = true;
        }
    }
    if (!cached)
        return keep_shared(log, block);

    /* With this CPU's lock let go of: what kept_blocks_ has no room for is
       unmapped, and the shootdown waits on every other CPU */
    for (ulong i = 0; i < spilled; i++)
    {
        if (!keep_shared(log, spill[i]) && !fixed_pg_alloc_[log].free(spill[i]))
            PANIC("Can't free kept block 0x%p", spill[i]);
    }
    return true;
}

bool PageAllocatorImpl::keep_shared(size_t log, Kept* block)
{
    auto& list = kept_blocks_[log];

    stdlib::AutoLock lock(list.lock);
    if (list.count >= list.limit)
        return false;

    block->next = list.head;
    block->mark = KEPT_MARK;
    list.head = block;
    list.count++;
    return true;
}

/* Every CPU's cache and kept_blocks_, each under its own lock -- never two at
   once. Only for a block that carries the mark, which a block handed out
   does not, so a free reaches here for a double free or a block whose
   owner happened to write the mark's very words. */
bool PageAllocatorImpl::is_kept(size_t log, Kept* block)
{
    if (log < CPU_KEPT_LOGS)
    {
        for (auto& cpu : cpu_kept_blocks_)
        {
            stdlib::AutoLock lock(cpu.lock);
            const CpuKeptSize& size = cpu.sizes[log];
            for (ulong i = 0; i < size.count; i++)
                if (size.blocks[i] == block)
                    return true;
        }
    }

    auto& list = kept_blocks_[log];
    stdlib::AutoLock lock(list.lock);
    for (Kept* kept = list.head; kept != nullptr; kept = kept->next)
        if (kept == block)
            return true;
    return false;
}

void* PageAllocatorImpl::take_kept(size_t log)
{
    BUG_ON(log >= stdlib::array_size(kept_blocks_));
    Kept* block = nullptr;

    CpuKept* cpu = this_cpu_kept(log);
    if (cpu != nullptr)
    {
        stdlib::AutoLock lock(cpu->lock);
        CpuKeptSize& size = cpu->sizes[log];
        if (size.count > 0)
            block = size.blocks[--size.count];
    }

    if (block == nullptr)
    {
        auto& list = kept_blocks_[log];
        stdlib::AutoLock lock(list.lock);
        block = list.head;
        if (block == nullptr)
            return nullptr;
        list.head = block->next;
        list.count--;
    }

    /* The caller's now: unmarked, so that its free does not go looking */
    block->mark = 0;
    return block;
}

/* Each list is taken whole under its lock and released with the lock let
   go of: a release unmaps, and the shootdown waits on every other CPU */
bool PageAllocatorImpl::trim()
{
    bool trimmed = false;
    for (auto& cpu : cpu_kept_blocks_)
    {
        for (size_t log = 0; log < CPU_KEPT_LOGS; log++)
        {
            Kept* blocks[CPU_KEPT_PAGES];
            ulong count;
            {
                stdlib::AutoLock lock(cpu.lock);
                CpuKeptSize& size = cpu.sizes[log];
                count = size.count;
                for (ulong i = 0; i < count; i++)
                    blocks[i] = size.blocks[i];
                size.count = 0;
            }

            for (ulong i = 0; i < count; i++)
            {
                if (!fixed_pg_alloc_[log].free(blocks[i]))
                    PANIC("Can't free kept block 0x%p", blocks[i]);
                trimmed = true;
            }
        }
    }

    for (size_t log = 0; log < stdlib::array_size(kept_blocks_); log++)
    {
        Kept* head;
        {
            stdlib::AutoLock lock(kept_blocks_[log].lock);
            head = kept_blocks_[log].head;
            kept_blocks_[log].head = nullptr;
            kept_blocks_[log].count = 0;
        }

        while (head != nullptr)
        {
            Kept* next = head->next;
            if (!fixed_pg_alloc_[log].free(head))
                PANIC("Can't free kept block 0x%p", head);
            head = next;
            trimmed = true;
        }
    }
    return trimmed;
}

void* PageAllocatorImpl::alloc_map_pages(size_t num_pages, ulong* phys_addr)
{
    BUG_ON(num_pages == 0);

    size_t log = stdlib::log2(num_pages);
    if (log >= stdlib::array_size(fixed_pg_alloc_))
        return nullptr;

    /* Kept blocks hold pages and this size's VA both: once more without
       them before refusing */
    void* result = try_alloc_map_pages(log, phys_addr);
    if (result == nullptr && trim())
        result = try_alloc_map_pages(log, phys_addr);
    return result;
}

void* PageAllocatorImpl::try_alloc_map_pages(size_t log, ulong* phys_addr)
{
    size_t rounded_pages = 1UL << log;
    auto& pt = PageTable::get_instance();
    Page* pages = pt.alloc_contiguous_pages(rounded_pages);
    if (!pages)
        return nullptr;

    void* result = fixed_pg_alloc_[log].map(pages);
    if (!result)
    {
        for (size_t i = 0; i < rounded_pages; i++)
            pt.free_page(&pages[i]);
        return nullptr;
    }
    *phys_addr = pages->get_phy_address();
    return result;
}

/* Not kept: a DMA buffer's pages go back to the page allocator as they
   always have */
void PageAllocatorImpl::unmap_free_pages(void* ptr)
{
    release(ptr);
}

void* PageAllocatorImpl::map_pages(size_t num_pages, ulong* phys_addrs)
{
    BUG_ON(num_pages == 0);

    size_t log = stdlib::log2(num_pages);
    if (log >= stdlib::array_size(fixed_pg_alloc_))
        return nullptr;

    return fixed_pg_alloc_[log].map_phys(phys_addrs, num_pages);
}

void PageAllocatorImpl::unmap_pages(void* ptr, size_t num_pages)
{
    BUG_ON(num_pages == 0);

    size_t log = stdlib::log2(num_pages);
    if (log >= stdlib::array_size(fixed_pg_alloc_))
    {
        PANIC("Can't unmap addr 0x%p num_pages %lu", ptr, num_pages);
        return;
    }

    if (!fixed_pg_alloc_[log].unmap(ptr, num_pages))
        PANIC("Can't unmap addr 0x%p num_pages %lu", ptr, num_pages);
}

void* PageAllocatorImpl::map_large_pages(size_t num_pages, ulong* phys_addrs)
{
    BUG_ON(num_pages == 0);

    if (num_pages > PageTable::MAX_LARGE_MAP_PAGES)
        return nullptr;

    return large_pg_alloc_.map_phys(phys_addrs, num_pages);
}

void PageAllocatorImpl::unmap_large_pages(void* ptr, size_t num_pages)
{
    BUG_ON(num_pages == 0 || num_pages > PageTable::MAX_LARGE_MAP_PAGES);

    if (!large_pg_alloc_.unmap(ptr, num_pages))
        PANIC("Can't unmap large run 0x%p num_pages %lu", ptr, num_pages);
}

}
}