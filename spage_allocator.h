#pragma once


#include "list_entry.h"
#include "spin_lock.h"
#include "page_allocator.h"
#include "const.h"

namespace Kernel
{

namespace Core
{

class SPageAllocator : public PageAllocator
{
public:
	static SPageAllocator& GetInstance()
	{
		static SPageAllocator instance;
		return instance;
	}

    virtual void* Alloc() override;
    virtual void Free(void* page) override;

private:
    SPageAllocator();
    virtual ~SPageAllocator();

    SPageAllocator(const SPageAllocator& other) = delete;
    SPageAllocator(SPageAllocator&& other) = delete;
    SPageAllocator& operator=(const SPageAllocator& other) = delete;
    SPageAllocator& operator=(SPageAllocator&& other) = delete;

    using ListEntry = Shared::ListEntry;

    static const size_t MaxPages = 1024;

    u8 Page[MaxPages * PAGE_SIZE] __attribute__((aligned(PAGE_SIZE)));
    ListEntry PageList;
    ulong Usage;

    SpinLock Lock;
};

}

}