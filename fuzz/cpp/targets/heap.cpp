// The kernel heap (mm/allocator.cpp, pool.cpp, page_allocator.cpp,
// va_allocator.cpp): Mm::Alloc and Mm::Free, the pools of every size class
// with their lists threaded through the blocks themselves, the page runs
// behind them, the VA allocators' bitmaps, and the three ways a driver maps
// pages -- everything every allocation of the kernel goes through, and the
// allocation failures nothing else makes happen.
//
// Under them, the page table as page_table.h promises it (a page run
// allocated, mapped, unmapped, freed), over pages that are only names and a
// VA region of the host's own: a VA the heap maps is host memory it may use,
// zeroed as the kernel's are; one it does not is poisoned for ASan, so the
// heap reaching past what it mapped is a report. Pages and maps fail on the
// input's word.
//
// A random sequence of allocations of every size -- the pools', the edges
// between them, whole pages, runs past the largest -- frees, page runs and
// driver mappings is held to what an allocator is: every block it hands out
// the size asked for, aligned, inside what is mapped and nobody else's;
// what is written into a block there when it is freed; nothing refused while
// there is memory to give; and once everything is freed, every page back
// but the bitmaps', whatever failed on the way.
#include "host.h"

/* The heap's singletons have private constructors, which Reset runs again
   in place. */
#define private public
#include <mm/allocator.h>
#include <mm/page_allocator.h>
#include <mm/page_table.h>
#include <mm/va_allocator.h>
#undef private

#include <kernel/cpu.h>

#include "fuzz.h"
#include "kernel.h"

#include <sanitizer/asan_interface.h>
#include <sys/mman.h>

using namespace kernel;
using kernel::mm::AllocatorImpl;
using kernel::mm::Page;
using kernel::mm::PageAllocatorImpl;
using kernel::mm::PageTable;

namespace
{

const ulong PAGE_BYTES = consts::PAGE_SIZE;

/* The host VA the heap's VA allocators are given: reserved once, used a
   few megabytes of per input, and the large runs' window past them. */
const ulong REGION_BYTES = 2UL << 30;
uint8_t* region;

/* The pages: names, a Page each, as PageArray has them */
struct PageSet
{
    Page* array = nullptr;
    ulong count = 0;

    Page* data() { return array; }
    ulong size() const { return count; }
    Page& operator[](ulong i) { return array[i]; }
};

struct Pages
{
    PageSet all;
    std::vector<bool> free;
    /* Free pages, most recently freed on top; some may have been taken by
       AllocContiguousPages since, and are passed over */
    std::vector<ulong> stack;
    /* How many VAs each page is mapped at */
    std::vector<ulong> map_count;
    ulong free_count = 0;
    long fail_after = -1;
    long map_fail_after = -1;
    /* va -> page index, for every page mapped */
    std::map<ulong, ulong> mapped;
} page_model;

bool take(long& fail_after)
{
    if (fail_after == 0)
        return false;
    if (fail_after > 0)
        fail_after--;
    return true;
}

ulong index_of(Page* page)
{
    ulong i = static_cast<ulong>(page - page_model.all.data());
    INVARIANT(i < page_model.all.size(), "a page that is no page: %p", page);
    return i;
}

void map_one(ulong va, ulong index)
{
    INVARIANT(va % PAGE_BYTES == 0, "a map at 0x%lx, not a page's start", va);
    INVARIANT(va >= reinterpret_cast<ulong>(region) && va + PAGE_BYTES <= reinterpret_cast<ulong>(region) + REGION_BYTES,
        "a map at 0x%lx, outside the VA the heap was given", va);
    INVARIANT(!page_model.free[index], "free page %lu mapped at 0x%lx", index, va);
    INVARIANT(page_model.mapped.insert({va, index}).second, "0x%lx mapped twice", va);
    page_model.map_count[index]++;
    ASAN_UNPOISON_MEMORY_REGION(reinterpret_cast<void*>(va), PAGE_BYTES);
    memset(reinterpret_cast<void*>(va), 0, PAGE_BYTES);
}

ulong unmap_one(ulong va)
{
    auto it = page_model.mapped.find(va);
    INVARIANT(it != page_model.mapped.end(), "an unmap of 0x%lx, which is not mapped", va);
    ulong index = it->second;
    page_model.mapped.erase(it);
    page_model.map_count[index]--;
    ASAN_POISON_MEMORY_REGION(reinterpret_cast<void*>(va), PAGE_BYTES);
    return index;
}

}

/* ---- the page table, as the heap uses it ---- */

namespace kernel
{
namespace mm
{

PageTable::PageTable()
{
}

PageTable::~PageTable()
{
}

Page* PageTable::alloc_page()
{
    INVARIANT(fuzz::held_spin_locks() == 0, "a page allocated with a spin lock held");
    if (!take(page_model.fail_after) || page_model.free_count == 0)
        return nullptr;
    while (!page_model.stack.empty())
    {
        ulong i = page_model.stack.back();
        page_model.stack.pop_back();
        if (page_model.free[i])
        {
            page_model.free[i] = false;
            page_model.free_count--;
            return &page_model.all[i];
        }
    }
    INVARIANT(false, "%lu pages free and none on the stack", page_model.free_count);
    return nullptr;
}

Page* PageTable::alloc_contiguous_pages(ulong count)
{
    if (!take(page_model.fail_after) || count == 0 || count > MAX_CONTIGUOUS_PAGES)
        return nullptr;
    for (ulong i = 0; i + count <= page_model.all.size(); i++)
    {
        ulong n = 0;
        while (n < count && page_model.free[i + n])
            n++;
        if (n == count)
        {
            for (ulong j = 0; j < count; j++)
                page_model.free[i + j] = false;
            page_model.free_count -= count;
            return &page_model.all[i];
        }
        i += n;
    }
    return nullptr;
}

void PageTable::free_page(Page* page)
{
    ulong i = index_of(page);
    INVARIANT(!page_model.free[i], "page %lu freed twice", i);
    INVARIANT(page_model.map_count[i] == 0, "page %lu freed while mapped", i);
    page_model.free[i] = true;
    page_model.free_count++;
    page_model.stack.push_back(i);
}

ulong PageTable::get_free_pages_count()
{
    return page_model.free_count;
}

ulong PageTable::get_va_end()
{
    return reinterpret_cast<ulong>(region);
}

bool PageTable::map_page(ulong virt_addr, Page* page)
{
    return map_pages(virt_addr, &page, 1);
}

bool PageTable::map_pages(ulong virt_addr, Page* const* pages, size_t count)
{
    INVARIANT(fuzz::held_spin_locks() == 0, "pages mapped with a spin lock held");
    if (!take(page_model.map_fail_after))
        return false;
    for (size_t i = 0; i < count; i++)
        map_one(virt_addr + i * PAGE_BYTES, index_of(pages[i]));
    return true;
}

bool PageTable::map_contiguous_pages(ulong virt_addr, Page* pages, size_t count)
{
    if (!take(page_model.map_fail_after))
        return false;
    for (size_t i = 0; i < count; i++)
        map_one(virt_addr + i * PAGE_BYTES, index_of(&pages[i]));
    return true;
}

bool PageTable::map_phys_pages(ulong virt_addr, const ulong* phy_addrs, size_t count)
{
    if (!take(page_model.map_fail_after))
        return false;
    for (size_t i = 0; i < count; i++)
    {
        INVARIANT(phy_addrs[i] % PAGE_BYTES == 0 && phy_addrs[i] / PAGE_BYTES < page_model.all.size(),
            "a map of physical 0x%lx, which is no page", phy_addrs[i]);
        map_one(virt_addr + i * PAGE_BYTES, phy_addrs[i] / PAGE_BYTES);
    }
    return true;
}

void PageTable::unmap_pages(ulong virt_addr, size_t count, bool free_pages)
{
    for (size_t i = 0; i < count; i++)
    {
        ulong index = unmap_one(virt_addr + i * PAGE_BYTES);
        if (free_pages)
            free_page(&page_model.all[index]);
    }
}

Page* PageTable::unmap_page(ulong virt_addr)
{
    return &page_model.all[unmap_one(virt_addr)];
}

}

void CpuTable::invalidate_tlb_range(ulong virt_addr, ulong count)
{
    (void)virt_addr;
    (void)count;
}

}

namespace
{

/* ---- the model ---- */

struct Block
{
    ulong size;
    uint8_t mark;
    enum Kind
    {
        Heap,
        Run,    /* AllocMapPages */
        Driver, /* MapPages of the sequence's own pages */
        Large,  /* MapLargePages */
    } how;
    std::vector<ulong> pages;
};

std::map<ulong, Block> live;
ulong baseline;

AllocatorImpl& tested_heap()
{
    return AllocatorImpl::get_instance(&PageAllocatorImpl::get_instance());
}

bool is_mapped(ulong va, ulong size)
{
    for (ulong p = va & ~(PAGE_BYTES - 1); p < va + size; p += PAGE_BYTES)
    {
        if (!page_model.mapped.count(p))
            return false;
    }
    return true;
}

void fill(ulong va, const Block& b)
{
    memset(reinterpret_cast<void*>(va), b.mark, b.size);
}

void check_fill(ulong va, const Block& b, const char* when)
{
    const uint8_t* p = reinterpret_cast<const uint8_t*>(va);
    for (ulong i = 0; i < b.size; i++)
        INVARIANT(p[i] == b.mark, "the block at 0x%lx of %lu bytes changed at +%lu %s", va, b.size, i, when);
}

void place(ulong va, Block b, const char* by)
{
    INVARIANT(is_mapped(va, b.size), "%s gave 0x%lx for %lu bytes, which is not all mapped", by, va, b.size);
    auto next = live.lower_bound(va);
    if (next != live.end())
        INVARIANT(va + b.size <= next->first, "%s gave [0x%lx, +%lu), which runs into the block at 0x%lx", by, va,
            b.size, next->first);
    if (next != live.begin())
    {
        auto prev = std::prev(next);
        INVARIANT(prev->first + prev->second.size <= va, "%s gave 0x%lx, inside the block at 0x%lx", by, va,
            prev->first);
    }
    fill(va, b);
    live[va] = b;
}

ulong pick_size(fuzz::Input& in)
{
    switch (in.u8() % 10)
    {
    case 0:
        return 1 + in.below(16);
    case 1:
    case 2:
        return 1 + in.below(256);
    case 3:
        return 1 + in.below(2048);
    case 4:
        /* The edge between the pools and whole pages */
        return 2032 + in.below(32);
    case 5:
        return PAGE_BYTES * (1 + in.below(4)) - 8 + in.below(16);
    case 6:
        return 1 + in.below(PAGE_BYTES * PageTable::MAX_CONTIGUOUS_PAGES + PAGE_BYTES);
    default:
        return 1 + in.below(512);
    }
}

/* Blocks of a fixed page allocator's VA nobody holds: its bitmap's zeros */
ulong va_free(ulong log)
{
    auto& va = PageAllocatorImpl::get_instance().fixed_pg_alloc_[log].va_alloc_;
    ulong free = 0;
    for (ulong i = 0; i < va.block_count_; i++)
        free += ((va.bitmap_ptr_[i / 8] >> (i % 8)) & 1) == 0;
    return free;
}

/* Blocks every CPU's cache holds */
ulong cached_count()
{
    ulong n = 0;
    for (auto& cache : tested_heap().caches_)
        for (auto& size : cache.sizes)
            n += size.count;
    return n;
}

/* Blocks the page allocator keeps, of every size */
ulong kept_count()
{
    auto& pa = PageAllocatorImpl::get_instance();
    ulong n = 0;
    for (auto& list : pa.kept_blocks_)
        n += list.count;
    for (auto& cpu : pa.cpu_kept_blocks_)
        for (auto& size : cpu.sizes)
            n += size.count;
    return n;
}

/* Blocks every CPU's cache of kept blocks holds */
ulong cpu_kept_count()
{
    ulong n = 0;
    for (auto& cpu : PageAllocatorImpl::get_instance().cpu_kept_blocks_)
        for (auto& size : cpu.sizes)
            n += size.count;
    return n;
}

/* A kept block as it must be, wherever it is kept: of its size's VA, all
   mapped, marked, and nobody's in use; and kept once */
void check_kept_block(PageAllocatorImpl& pa, ulong log, ulong va, std::set<ulong>& seen)
{
    auto* kept = reinterpret_cast<PageAllocatorImpl::Kept*>(va);
    INVARIANT(pa.fixed_pg_alloc_[log].contains(kept), "a kept block 0x%lx not of its size's VA", va);
    INVARIANT(is_mapped(va, (1UL << log) * PAGE_BYTES), "a kept block 0x%lx not all mapped", va);
    INVARIANT(kept->mark == PageAllocatorImpl::KEPT_MARK, "a kept block 0x%lx without its mark", va);
    auto after = live.upper_bound(va + (1UL << log) * PAGE_BYTES - 1);
    INVARIANT(after == live.begin() || std::prev(after)->first + std::prev(after)->second.size <= va,
        "a kept block 0x%lx that a block in use overlaps", va);
    INVARIANT(seen.insert(va).second, "the block 0x%lx kept twice", va);
}

/* What the heap keeps for its next allocations, between two operations:
   no more than its bounds, each kept block whole -- mapped, marked, its own
   and nobody's in use -- and each pool's kept pages empty */
void check_kept()
{
    auto& pa = PageAllocatorImpl::get_instance();
    std::set<ulong> kept;
    for (ulong log = 0; log < stdlib::array_size(pa.kept_blocks_); log++)
    {
        auto& list = pa.kept_blocks_[log];
        INVARIANT(list.count <= list.limit, "%lu blocks of %lu pages kept, past the bound of %lu", list.count,
            1UL << log, list.limit);
        ulong n = 0;
        for (auto* block = list.head; block != nullptr; block = block->next)
        {
            INVARIANT(++n <= list.count, "the list of kept %lu-page blocks is longer than its count %lu",
                1UL << log, list.count);
            check_kept_block(pa, log, reinterpret_cast<ulong>(block), kept);
        }
        INVARIANT(n == list.count, "%lu kept %lu-page blocks on a list that counts %lu", n, 1UL << log,
            list.count);
    }

    /* Each CPU's: within its bound, which is within KeptBlocks' */
    for (ulong cpu = 0; cpu < stdlib::array_size(pa.cpu_kept_blocks_); cpu++)
    {
        for (ulong log = 0; log < PageAllocatorImpl::CPU_KEPT_LOGS; log++)
        {
            auto& size = pa.cpu_kept_blocks_[cpu].sizes[log];
            INVARIANT(size.count <= size.limit && size.limit <= pa.kept_blocks_[log].limit,
                "CPU %lu keeps %lu blocks of %lu pages, its bound %lu and the shared one %lu", cpu, size.count,
                1UL << log, size.limit, pa.kept_blocks_[log].limit);
            for (ulong i = 0; i < size.count; i++)
                check_kept_block(pa, log, reinterpret_cast<ulong>(size.blocks[i]), kept);
        }
    }

    auto& heap = tested_heap();
    for (auto& pool : heap.pool_)
        INVARIANT(pool.empty_pages_ <= kernel::mm::Pool::EMPTY_PAGES_KEPT, "a pool of %lu-byte blocks keeps %lu empty "
            "pages", pool.block_size_, pool.empty_pages_);

    /* Every CPU's cache: within its bound, each block in it free -- its tag
       the pool's FreedTag -- in no other cache and nobody's in use (whose
       address is the block's past the heap's header) */
    std::set<ulong> cached;
    for (ulong cpu = 0; cpu < stdlib::array_size(heap.caches_); cpu++)
    {
        for (ulong p = 0; p < AllocatorImpl::POOL_COUNT; p++)
        {
            auto& size = heap.caches_[cpu].sizes[p];
            INVARIANT(size.count <= size.limit, "CPU %lu caches %lu blocks of pool %lu, past its bound of %lu", cpu,
                size.count, p, size.limit);
            for (ulong i = 0; i < size.count; i++)
            {
                ulong block = reinterpret_cast<ulong>(size.blocks[i]);
                auto* header = reinterpret_cast<kernel::mm::Pool::Block*>(block) - 1;
                INVARIANT(static_cast<ulong>(header->tag.get()) == kernel::mm::Pool::FREED_TAG,
                    "CPU %lu caches 0x%lx, whose tag 0x%lx is not a free block's", cpu, block,
                    static_cast<ulong>(header->tag.get()));
                INVARIANT(cached.insert(block).second, "0x%lx cached twice", block);
                INVARIANT(!live.count(block + sizeof(AllocatorImpl::Header)), "CPU %lu caches 0x%lx, which is in use",
                    cpu, block);
            }
        }
    }
}

void alloc(fuzz::Input& in)
{
    ulong size = pick_size(in);
    /* Rust's allocations, which ask for no contents in particular */
    const bool uninit = in.boolean();
    ulong kept_before = kept_count();
    ulong cached_before = cached_count();
    ulong cpu_kept_before = cpu_kept_count();
    void* p = uninit ? tested_heap().alloc_uninit(size, 0x54657374) : tested_heap().alloc(size, 0x54657374);
    if (cpu_kept_count() < cpu_kept_before)
        fuzz::reached("a kept block from a CPU's own");
    if (kept_count() < kept_before)
        fuzz::reached(uninit ? "a kept block handed out as it was" : "a kept block handed out zeroed");
    if (cached_count() < cached_before)
        fuzz::reached("a block from a CPU's cache");
    if (p == nullptr)
    {
        /* Past the largest run, no page or no VA for one -- a pool's block
           may need a page of its own -- or a failure on the input's word */
        ulong pages = (size + 8 >= PAGE_BYTES / 2) ? (size + PAGE_BYTES - 1) / PAGE_BYTES : 1;
        ulong log = stdlib::log2(pages);
        INVARIANT(pages > PageTable::MAX_CONTIGUOUS_PAGES || page_model.free_count < (1UL << log) || va_free(log) == 0 ||
            page_model.fail_after >= 0 || page_model.map_fail_after >= 0, "Alloc(%lu) refused with %lu pages and %lu blocks of VA free",
            size, page_model.free_count, va_free(log));
        fuzz::reached("an allocation refused");
        return;
    }
    ulong va = reinterpret_cast<ulong>(p);
    INVARIANT(va % 8 == 0, "Alloc(%lu) gave 0x%lx, not 8-aligned", size, va);
    const bool pages = size + 8 >= PAGE_BYTES / 2;
    if (pages && !uninit)
    {
        /* Zeroed, as fresh pages are -- a block the page allocator kept
           from a free included, whatever was written into it before */
        INVARIANT(is_mapped(va, size), "Alloc gave 0x%lx for %lu bytes, which is not all mapped", va, size);
        const uint8_t* bytes = reinterpret_cast<const uint8_t*>(va);
        for (ulong i = 0; i < size; i++)
            INVARIANT(bytes[i] == 0, "Alloc(%lu) gave 0x%lx with a byte not zero at +%lu", size, va, i);
    }
    place(va, {size, static_cast<uint8_t>(1 + in.below(255)), Block::Heap, {}}, "Alloc");
    fuzz::reached(pages ? "a page allocation" : "a pool allocation");
}

void free_one(fuzz::Input& in)
{
    if (live.empty())
        return;
    auto it = live.begin();
    std::advance(it, in.below(live.size()));
    ulong va = it->first;
    Block b = it->second;
    check_fill(va, b, "before its free");
    live.erase(it);
    auto& pa = PageAllocatorImpl::get_instance();
    switch (b.how)
    {
    case Block::Heap:
    {
        ulong cached_before = cached_count();
        tested_heap().free(reinterpret_cast<void*>(va));
        if (cached_count() < cached_before)
            fuzz::reached("a CPU's cache spilled to its pool");
        break;
    }
    case Block::Run:
        pa.unmap_free_pages(reinterpret_cast<void*>(va));
        break;
    case Block::Driver:
        pa.unmap_pages(reinterpret_cast<void*>(va), b.pages.size());
        for (ulong i : b.pages)
            PageTable::get_instance().free_page(&page_model.all[i]);
        break;
    case Block::Large:
        pa.unmap_large_pages(reinterpret_cast<void*>(va), b.pages.size());
        for (ulong i : b.pages)
            PageTable::get_instance().free_page(&page_model.all[i]);
        break;
    }
}

void page_run(fuzz::Input& in)
{
    ulong count = 1 + in.below(in.chance(32) ? 200 : 12);
    ulong phys = 0;
    void* p = PageAllocatorImpl::get_instance().alloc_map_pages(count, &phys);
    if (p == nullptr)
    {
        fuzz::reached("a page run refused");
        return;
    }
    ulong va = reinterpret_cast<ulong>(p);
    INVARIANT(va % PAGE_BYTES == 0, "AllocMapPages gave 0x%lx", va);
    /* The run is contiguous, from phys on, and all of it mapped in order */
    ulong rounded = 1;
    while (rounded < count)
        rounded <<= 1;
    std::vector<ulong> pages;
    for (ulong i = 0; i < rounded; i++)
    {
        auto m = page_model.mapped.find(va + i * PAGE_BYTES);
        INVARIANT(m != page_model.mapped.end() && m->second == phys / PAGE_BYTES + i,
            "page %lu of the run at 0x%lx is not physical 0x%lx", i, va, phys + i * PAGE_BYTES);
        pages.push_back(m->second);
    }
    place(va, {rounded * PAGE_BYTES, static_cast<uint8_t>(1 + in.below(255)), Block::Run, pages}, "AllocMapPages");
    fuzz::reached("a page run");
}

void driver(fuzz::Input& in)
{
    const bool large = in.chance(32);
    ulong count = large ? 1 + in.below(300) : 1 + in.below(8);
    std::vector<ulong> pages;
    std::vector<ulong> phys;
    for (ulong i = 0; i < count; i++)
    {
        Page* page = PageTable::get_instance().alloc_page();
        if (page == nullptr)
            break;
        pages.push_back(index_of(page));
        phys.push_back(index_of(page) * PAGE_BYTES);
    }
    void* p = nullptr;
    if (!pages.empty())
    {
        auto& pa = PageAllocatorImpl::get_instance();
        p = large ? pa.map_large_pages(pages.size(), phys.data()) : pa.map_pages(pages.size(), phys.data());
    }
    if (p == nullptr)
    {
        for (ulong i : pages)
            PageTable::get_instance().free_page(&page_model.all[i]);
        fuzz::reached("a driver's map refused");
        return;
    }
    ulong va = reinterpret_cast<ulong>(p);
    for (ulong i = 0; i < pages.size(); i++)
    {
        auto m = page_model.mapped.find(va + i * PAGE_BYTES);
        INVARIANT(m != page_model.mapped.end() && m->second == pages[i], "a driver's page %lu mapped elsewhere", i);
    }
    place(va, {pages.size() * PAGE_BYTES, static_cast<uint8_t>(1 + in.below(255)), large ? Block::Large : Block::Driver,
               pages}, large ? "MapLargePages" : "MapPages");
    fuzz::reached(large ? "a large driver's map" : "a driver's map");
}

void boot(fuzz::Input& in)
{
    /* 32 MiB and up: a smaller machine gives the largest runs' allocator no
       VA to set up over, and the kernel would not boot on it */
    ulong pages = 8192 + in.below(8192);
    page_model.all.array = new Page[pages];
    page_model.all.count = pages;
    for (ulong i = 0; i < pages; i++)
        page_model.all[i].init(i * PAGE_BYTES);
    page_model.free.assign(pages, true);
    page_model.map_count.assign(pages, 0);
    page_model.free_count = pages;
    for (ulong i = pages; i-- > 0;)
        page_model.stack.push_back(i);
    /* The fixed allocators' ranges and the first of the large runs' window:
       poisoned until mapped */
    ASAN_POISON_MEMORY_REGION(region, 64UL << 20);
    INVARIANT(PageAllocatorImpl::get_instance().setup(), "the page allocator would not set up over %lu pages", pages);
    tested_heap();
    baseline = pages - page_model.free_count;
}

void run(fuzz::Input& in)
{
    boot(in);
    for (int ops = 0; ops < 200 && in.more(); ops++)
    {
        /* Which CPU the operation runs on: a block freed on another than
           the one it was taken on moves between their caches; past the
           caches' CPUs there is none, and the pools are reached directly */
        if (in.chance(32))
            fuzz::current_cpu = in.chance(16) ? kernel::mm::CACHE_CPUS + in.below(4) : in.below(4);
        switch (in.u8() % 13)
        {
        case 12:
            /* What the heap keeps for its next allocations given back,
               with blocks still out */
            if (tested_heap().trim())
                fuzz::reached("a trim that gave something back");
            break;
        case 0:
        case 1:
        case 2:
        case 3:
        case 4:
            alloc(in);
            break;
        case 5:
        case 6:
        case 7:
            free_one(in);
            break;
        case 8:
            page_run(in);
            break;
        case 9:
            driver(in);
            break;
        case 10:
            /* Pages and maps that fail, from a few calls on */
            if (!in.chance(48))
                break;
            if (in.boolean())
                page_model.fail_after = in.below(4);
            else
                page_model.map_fail_after = in.below(3);
            fuzz::reached("failures armed");
            break;
        default:
            page_model.fail_after = -1;
            page_model.map_fail_after = -1;
            break;
        }
        fuzz::check_no_locks_held("an operation");
        check_kept();
    }

    /* Everything freed and the heap trimmed: every block as it was written,
       and every page back but the VA allocators' bitmaps -- none left in
       a pool's kept page or among the page allocator's kept blocks */
    page_model.fail_after = -1;
    page_model.map_fail_after = -1;
    for (auto& l : live)
        check_fill(l.first, l.second, "at the end");
    while (!live.empty())
        free_one(in);
    tested_heap().trim();
    INVARIANT(page_model.all.size() - page_model.free_count == baseline, "%lu pages still out once everything is freed, where the "
        "bitmaps hold %lu", page_model.all.size() - page_model.free_count, baseline);
    INVARIANT(!tested_heap().trim(), "a second trim found something the first left");
}

void reset()
{
    for (auto& m : page_model.mapped)
        ASAN_POISON_MEMORY_REGION(reinterpret_cast<void*>(m.first), PAGE_BYTES);
    delete[] page_model.all.array;
    page_model = Pages();
    live.clear();
    auto& heap = AllocatorImpl::get_instance(&PageAllocatorImpl::get_instance());
    heap.~AllocatorImpl();
    new (&heap) AllocatorImpl(&PageAllocatorImpl::get_instance());
    auto& pa = PageAllocatorImpl::get_instance();
    pa.~PageAllocatorImpl();
    new (&pa) PageAllocatorImpl();
}

/* The region, once: host memory the heap's VAs are in */
struct RegionInit
{
    RegionInit()
    {
        void* m = mmap(nullptr, REGION_BYTES + (16UL << 20), PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0);
        INVARIANT(m != MAP_FAILED, "no host VA for the heap");
        region = reinterpret_cast<uint8_t*>((reinterpret_cast<ulong>(m) + (16UL << 20) - 1) & ~((16UL << 20) - 1));
    }
} the_region;

}

const fuzz::Target fuzz::THE_TARGET = {"heap", reset, run, 2048, 5000};
