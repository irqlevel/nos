#pragma once

#include <include/const.h>
#include <kernel/spin_lock.h>
#include <lib/list_entry.h>

#include "block_allocator.h"
#include "cpu_cache.h"
#include "va_allocator.h"
#include "page_table.h"

namespace kernel
{

namespace mm
{

struct Page;

class PageAllocator
{
public:
    /* zero: whether the block must come back zeroed, as fresh pages are */
    virtual void* alloc(size_t num_pages, bool zero) = 0;
    virtual void free(void* ptr) = 0;
    virtual void* alloc_map_pages(size_t num_pages, ulong* phys_addr) = 0;
    virtual void unmap_free_pages(void* ptr) = 0;
    virtual void* map_pages(size_t num_pages, ulong* phys_addrs) = 0;
    virtual void unmap_pages(void* ptr, size_t num_pages) = 0;
    virtual bool trim() = 0;
};

class FixedPageAllocator
{
public:
    FixedPageAllocator();
    virtual ~FixedPageAllocator();

    static const size_t MAX_PAGE_COUNT = PageTable::MAX_CONTIGUOUS_PAGES;

    bool setup(ulong start, ulong end, ulong page_count);

    void* alloc();
    void* map(Page* pages);
    void* map_phys(ulong* phys_addrs, size_t count);
    bool free(void* addr);
    bool unmap(void* addr, size_t count);
    bool contains(void* addr);

private:
    FixedPageAllocator(const FixedPageAllocator& other) = delete;
    FixedPageAllocator(FixedPageAllocator&& other) = delete;
    FixedPageAllocator& operator=(const FixedPageAllocator& other) = delete;
    FixedPageAllocator& operator=(FixedPageAllocator&& other) = delete;

    VaAllocator va_alloc_;
    ulong page_count_;
};

/* final, with a destructor that is not virtual: the per-CPU caches below are
   aligned to cache lines, which makes the class over-aligned, and a virtual
   destructor of an over-aligned class calls the aligned operator delete the
   kernel does not have. The one instance is a static nobody deletes. */
class PageAllocatorImpl final : public PageAllocator
{
public:
	static PageAllocatorImpl& get_instance()
	{
		static PageAllocatorImpl instance;
		return instance;
	}

    bool setup();

    virtual void* alloc(size_t num_pages, bool zero) override;
    virtual void free(void* pages) override;
    virtual void* alloc_map_pages(size_t num_pages, ulong* phys_addr) override;
    virtual void unmap_free_pages(void* ptr) override;
    virtual void* map_pages(size_t num_pages, ulong* phys_addrs) override;
    virtual void unmap_pages(void* ptr, size_t num_pages) override;

    /* MapPages for runs past the largest block: up to
       PageTable::MaxLargeMapPages pages, from a window of blocks that big */
    void* map_large_pages(size_t num_pages, ulong* phys_addrs);
    void unmap_large_pages(void* ptr, size_t num_pages);

    /* Hand every block the caches below hold back to the page allocator;
       whether there was one. Called without a spin lock held: it unmaps,
       and an unmap shoots down every CPU's TLB. */
    virtual bool trim() override;

private:
    PageAllocatorImpl();
    ~PageAllocatorImpl();

    /* Free and Alloc keep a block of the heap's freed with its pages still
       mapped, and hand it out again: a block that comes back costs neither
       a map nor -- on x86, where each unmap is an IPI to every other CPU and
       a wait for all of them -- a shootdown. A kept block is threaded
       through its own first words, and marked, so that a second free of it
       is caught rather than handing it out twice. Each size has a bound, in
       pages, that RAM sets at Setup; Trim, and an allocation that finds no
       page, empty them. AllocMapPages's runs are not kept: a DMA buffer goes
       back as it always has. */
    struct Kept
    {
        Kept* next;
        ulong mark;
    };

    struct KeptList
    {
        SpinLock lock;
        Kept* head = nullptr;
        ulong count = 0;
        /* Blocks, not pages; 0 until Setup, which keeps nothing */
        ulong limit = 0;
    };

    static constexpr ulong KEPT_MARK = 0x4B4550544B455054UL;

    /* The most pages each size keeps: a thousandth of the pages free at
       Setup, and never more than 512 KiB -- 4 MiB over all eight sizes */
    static constexpr ulong KEPT_PAGES_MAX = 128;
    static constexpr ulong KEPT_RAM_SHARE = 1024;

    /* In front of KeptBlocks, for the three smallest sizes -- a page, two,
       four -- a few kept blocks a CPU, under a lock only that CPU takes
       (and Trim), as AllocatorImpl's caches are in front of its pools: four
       CPUs allocating pages took KeptBlocks' lock in turn, and a pair of
       4 KiB blocks cost ten times on four what it did on one (heapbench).
       A free a full cache cannot take spills the older half of it to
       KeptBlocks; an allocation an empty one cannot serve takes from
       there. A block in a CPU's cache is kept as one in KeptBlocks is:
       mapped, marked, nobody's. */
    static constexpr size_t CPU_KEPT_LOGS = 3;
    /* Pages a CPU keeps of each of those sizes: four blocks of a page, two
       of two, one of four -- and none past KeptBlocks' own bound */
    static constexpr ulong CPU_KEPT_PAGES = 4;

    struct CpuKeptSize
    {
        ulong count = 0;
        /* 0 until Setup, which keeps nothing */
        ulong limit = 0;
        Kept* blocks[CPU_KEPT_PAGES];
    };

    struct alignas(64) CpuKept
    {
        SpinLock lock;
        CpuKeptSize sizes[CPU_KEPT_LOGS];
    };

    /* This CPU's cache of kept blocks, for a size it has one of; nullptr
       otherwise */
    CpuKept* this_cpu_kept(size_t log);

    /* A freed block onto this CPU's cache, or KeptBlocks; false if neither
       took it */
    bool keep(size_t log, void* ptr);
    /* A block onto KeptBlocks; false if it is full */
    bool keep_shared(size_t log, Kept* block);
    /* Whether a block is kept, in any CPU's cache or KeptBlocks: the
       double-free check, for a block that carries the mark */
    bool is_kept(size_t log, Kept* block);
    /* A kept block, its mark cleared, from this CPU's cache or KeptBlocks;
       nullptr if there is none */
    void* take_kept(size_t log);
    /* Which of FixedPgAlloc holds addr; false if none does */
    bool class_of(void* addr, size_t& log);
    /* Unmap and free a block: what Free did before blocks were kept */
    void release(void* addr);
    /* One try at AllocMapPages, of 1 << log pages */
    void* try_alloc_map_pages(size_t log, ulong* phys_addr);

    PageAllocatorImpl(const PageAllocatorImpl& other) = delete;
    PageAllocatorImpl(PageAllocatorImpl&& other) = delete;
    PageAllocatorImpl& operator=(const PageAllocatorImpl& other) = delete;
    PageAllocatorImpl& operator=(PageAllocatorImpl&& other) = delete;

    static const size_t PAGE_LOG_LIMIT = stdlib::const_log2(PageTable::MAX_CONTIGUOUS_PAGES) + 1;

    FixedPageAllocator fixed_pg_alloc_[PAGE_LOG_LIMIT];

    /* How many large runs can be mapped at once -- one less, the first
       block holding the window's own bitmap. 1 GiB of VA, which costs page
       tables only where a run is mapped. */
    static const size_t LARGE_BLOCK_COUNT = 64;
    FixedPageAllocator large_pg_alloc_;

    KeptList kept_blocks_[PAGE_LOG_LIMIT];
    CpuKept cpu_kept_blocks_[CACHE_CPUS]; /* cpu_cache.h's */
};

}
}