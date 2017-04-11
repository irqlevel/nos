#pragma once

#include "page_allocator.h"

#include <include/const.h>
#include <kernel/spin_lock.h>
#include <lib/list_entry.h>

namespace Kernel
{

namespace Core
{

class SPageAllocator : public PageAllocator
{
public:
	static SPageAllocator& GetInstance(ulong pageStart, ulong pageEnd)
	{
		static SPageAllocator instance(pageStart, pageEnd);
        InstancePtr = &instance;
		return instance;
	}

	static SPageAllocator& GetInstance()
	{
		return *InstancePtr;
	}

    virtual void* Alloc() override;
    virtual void Free(void* page) override;

private:
    SPageAllocator(ulong pageStart, ulong pageEnd);
    virtual ~SPageAllocator();

    SPageAllocator(const SPageAllocator& other) = delete;
    SPageAllocator(SPageAllocator&& other) = delete;
    SPageAllocator& operator=(const SPageAllocator& other) = delete;
    SPageAllocator& operator=(SPageAllocator&& other) = delete;

    using ListEntry = Shared::ListEntry;

    ListEntry PageList;
    ulong Usage;
    ulong PageStart;
    ulong PageEnd;
    SpinLock Lock;

    static SPageAllocator* InstancePtr;

};

}

}