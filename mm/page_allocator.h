#pragma once

#include <include/const.h>
#include <kernel/spin_lock.h>
#include <lib/list_entry.h>

#include "block_allocator.h"

namespace Kernel
{

namespace Mm
{

class PageAllocator
{
public:
    virtual void* Alloc(size_t numPages) = 0;
    virtual void Free(void* ptr) = 0;
};

class FixedPageAllocator
{
public:
    FixedPageAllocator();
    virtual ~FixedPageAllocator();

    bool Setup(ulong vaStart, ulong vaEnd, ulong pageCount);

    void* Alloc();
    bool Free(void* addr);

private:
    FixedPageAllocator(const FixedPageAllocator& other) = delete;
    FixedPageAllocator(FixedPageAllocator&& other) = delete;
    FixedPageAllocator& operator=(const FixedPageAllocator& other) = delete;
    FixedPageAllocator& operator=(FixedPageAllocator&& other) = delete;

    u8 BlockBitmap[Const::PageSize] __attribute__((aligned(Const::PageSize)));

    SpinLock Lock;
    ulong VaStart;
    ulong VaEnd;
    ulong PageCount;
    ulong BlockCount;
    ulong BlockSize;
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

private:
    PageAllocatorImpl();
    virtual ~PageAllocatorImpl();

    PageAllocatorImpl(const PageAllocatorImpl& other) = delete;
    PageAllocatorImpl(PageAllocatorImpl&& other) = delete;
    PageAllocatorImpl& operator=(const PageAllocatorImpl& other) = delete;
    PageAllocatorImpl& operator=(PageAllocatorImpl&& other) = delete;

    static const size_t PageLogLimit = 4;

    FixedPageAllocator FixedPgAlloc[PageLogLimit];
};

}
}