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
    virtual void* Alloc(size_t numPages) = 0;
    virtual void Free(void* ptr) = 0;
    virtual void* AllocMapPages(size_t numPages, ulong* physAddr) = 0;
    virtual void UnmapFreePages(void* ptr) = 0;
    virtual void* MapPages(size_t numPages, ulong* physAddrs) = 0;
    virtual void UnmapPages(void* ptr, size_t numPages) = 0;
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

    virtual void* Alloc(size_t numPages) override;
    virtual void Free(void* pages) override;
    virtual void* AllocMapPages(size_t numPages, ulong* physAddr) override;
    virtual void UnmapFreePages(void* ptr) override;
    virtual void* MapPages(size_t numPages, ulong* physAddrs) override;
    virtual void UnmapPages(void* ptr, size_t numPages) override;

private:
    PageAllocatorImpl();
    virtual ~PageAllocatorImpl();

    PageAllocatorImpl(const PageAllocatorImpl& other) = delete;
    PageAllocatorImpl(PageAllocatorImpl&& other) = delete;
    PageAllocatorImpl& operator=(const PageAllocatorImpl& other) = delete;
    PageAllocatorImpl& operator=(PageAllocatorImpl&& other) = delete;

    static const size_t PageLogLimit = Stdlib::CLog2(PageTable::MaxContiguousPages) + 1;

    FixedPageAllocator FixedPgAlloc[PageLogLimit];
};

}
}