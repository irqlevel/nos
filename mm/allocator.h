#pragma once

#include "page_allocator.h"
#include "pool.h"

#include <include/const.h>
#include <kernel/spin_lock.h>
#include <lib/list_entry.h>

namespace Kernel
{

class Allocator
{
public:
	virtual void* Alloc(size_t size) = 0;
	virtual void Free(void* ptr) = 0;
};

class AllocatorImpl : public Allocator
{
public:
	static AllocatorImpl& GetInstance(PageAllocator& pageAllocator)
	{
		static AllocatorImpl instance(pageAllocator);
		return instance;
	}

	virtual void* Alloc(size_t size) override;
	virtual void Free(void* ptr) override;
private:
	AllocatorImpl(PageAllocator& pageAllocator);
	virtual ~AllocatorImpl();

	AllocatorImpl(const AllocatorImpl& other) = delete;
	AllocatorImpl(AllocatorImpl&& other) = delete;
	AllocatorImpl& operator=(const AllocatorImpl& other) = delete;
	AllocatorImpl& operator=(AllocatorImpl&& other) = delete;

	static const u32 Magic = 0xCBDECBDE;

	size_t Log2(size_t size);
	bool LogBySize(size_t size, size_t& log);

	struct Header {
		u32 Magic;
		u32 Size;
	};

	static const size_t StartLog = 3;
	static const size_t EndLog = Shared::PageShift - 1;

	Pool Pool[EndLog - StartLog + 1];
	PageAllocator& PageAllocator;
};

}
