#pragma once

#include "allocator.h"
#include "page_allocator.h"
#include "list_entry.h"

#include "spin_lock.h"
#include "spool.h"
#include "const.h"

namespace Kernel
{

namespace Core
{

class SAllocator : public Shared::Allocator
{
public:
	static SAllocator& GetInstance(PageAllocator& pageAllocator)
	{
		static SAllocator instance(pageAllocator);
		return instance;
	}

	virtual void* Alloc(size_t size) override;
	virtual void Free(void* ptr) override;
private:
	SAllocator(PageAllocator& pageAllocator);
	virtual ~SAllocator();

	SAllocator(const SAllocator& other) = delete;
	SAllocator(SAllocator&& other) = delete;
	SAllocator& operator=(const SAllocator& other) = delete;
	SAllocator& operator=(SAllocator&& other) = delete;

	static const u32 Magic = 0xCBDECBDE;

	size_t Log2(size_t size);
	bool LogBySize(size_t size, size_t& log);

	struct Header {
		u32 Magic;
		u32 Size;
	};

	static const size_t StartLog = 3;
	static const size_t EndLog = Shared::PageShift - 1;

	SPool Pool[EndLog - StartLog + 1];
	PageAllocator& PageAllocer;
};

}
}
