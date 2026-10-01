#pragma once

#include <include/const.h>
#include <kernel/spin_lock.h>
#include <lib/list_entry.h>

#include "block_allocator.h"
#include "va_allocator.h"
#include "page_table.h"

namespace Kernel
{

namespace Mm
{

struct Page;

class PageAllocator
{
public:
    /* zero: whether the block must come back zeroed, as fresh pages are */
    virtual void* Alloc(size_t numPages, bool zero) = 0;
    virtual void Free(void* ptr) = 0;
    virtual void* AllocMapPages(size_t numPages, ulong* physAddr) = 0;
    virtual void UnmapFreePages(void* ptr) = 0;
    virtual void* MapPages(size_t numPages, ulong* physAddrs) = 0;
    virtual void UnmapPages(void* ptr, size_t numPages) = 0;
    virtual bool Trim() = 0;
};

class FixedPageAllocator
{
public:
    FixedPageAllocator();
    virtual ~FixedPageAllocator();

    static const size_t MaxPageCount = PageTable::MaxContiguousPages;

    bool Setup(ulong vaStart, ulong vaEnd, ulong pageCount);

    void* Alloc();
    void* Map(Page* pages);
    void* MapPhys(ulong* physAddrs, size_t count);
    bool Free(void* addr);
    bool Unmap(void* addr, size_t count);
    bool Contains(void* addr);

private:
    FixedPageAllocator(const FixedPageAllocator& other) = delete;
    FixedPageAllocator(FixedPageAllocator&& other) = delete;
    FixedPageAllocator& operator=(const FixedPageAllocator& other) = delete;
    FixedPageAllocator& operator=(FixedPageAllocator&& other) = delete;

    VaAllocator VaAlloc;
    ulong PageCount;
};

class PageAllocatorImpl : public PageAllocator
{
public:
	static PageAllocatorImpl& GetInstance()
	{
		static PageAllocatorImpl Instance;
		return Instance;
	}

    bool Setup();

    virtual void* Alloc(size_t numPages, bool zero) override;
    virtual void Free(void* pages) override;
    virtual void* AllocMapPages(size_t numPages, ulong* physAddr) override;
    virtual void UnmapFreePages(void* ptr) override;
    virtual void* MapPages(size_t numPages, ulong* physAddrs) override;
    virtual void UnmapPages(void* ptr, size_t numPages) override;

    /* MapPages for runs past the largest block: up to
       PageTable::MaxLargeMapPages pages, from a window of blocks that big */
    void* MapLargePages(size_t numPages, ulong* physAddrs);
    void UnmapLargePages(void* ptr, size_t numPages);

    /* Hand every block the caches below hold back to the page allocator;
       whether there was one. Called without a spin lock held: it unmaps,
       and an unmap shoots down every CPU's TLB. */
    virtual bool Trim() override;

private:
    PageAllocatorImpl();
    virtual ~PageAllocatorImpl();

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
        Kept* Next;
        ulong Mark;
    };

    struct KeptList
    {
        SpinLock Lock;
        Kept* Head = nullptr;
        ulong Count = 0;
        /* Blocks, not pages; 0 until Setup, which keeps nothing */
        ulong Limit = 0;
    };

    static constexpr ulong KeptMark = 0x4B4550544B455054UL;

    /* The most pages each size keeps: a thousandth of the pages free at
       Setup, and never more than 512 KiB -- 4 MiB over all eight sizes */
    static constexpr ulong KeptPagesMax = 128;
    static constexpr ulong KeptRamShare = 1024;

    bool Keep(size_t log, void* ptr);
    void* TakeKept(size_t log);
    /* Which of FixedPgAlloc holds addr; false if none does */
    bool ClassOf(void* addr, size_t& log);
    /* Unmap and free a block: what Free did before blocks were kept */
    void Release(void* addr);
    /* One try at AllocMapPages, of 1 << log pages */
    void* TryAllocMapPages(size_t log, ulong* physAddr);

    PageAllocatorImpl(const PageAllocatorImpl& other) = delete;
    PageAllocatorImpl(PageAllocatorImpl&& other) = delete;
    PageAllocatorImpl& operator=(const PageAllocatorImpl& other) = delete;
    PageAllocatorImpl& operator=(PageAllocatorImpl&& other) = delete;

    static const size_t PageLogLimit = Stdlib::CLog2(PageTable::MaxContiguousPages) + 1;

    FixedPageAllocator FixedPgAlloc[PageLogLimit];

    /* How many large runs can be mapped at once -- one less, the first
       block holding the window's own bitmap. 1 GiB of VA, which costs page
       tables only where a run is mapped. */
    static const size_t LargeBlockCount = 64;
    FixedPageAllocator LargePgAlloc;

    KeptList KeptBlocks[PageLogLimit];
};

}
}