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

    BlockAllocatorImpl Balloc[PageLogLimit];

};

}
}