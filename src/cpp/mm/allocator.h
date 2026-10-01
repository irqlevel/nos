#pragma once

#include "cpu_cache.h"
#include "page_allocator.h"
#include "pool.h"

#include <include/const.h>
#include <kernel/spin_lock.h>
#include <lib/list_entry.h>

namespace Kernel
{

namespace Mm
{

class Allocator
{
public:
	virtual void* Alloc(size_t size, ulong tag) = 0;
	virtual void Free(void* ptr) = 0;
};

/* final, with a destructor that is not virtual: the per-CPU caches below are
   aligned to cache lines, which makes the class over-aligned, and a virtual
   destructor of an over-aligned class calls the aligned operator delete the
   kernel does not have. The one instance is a static nobody deletes. */
class AllocatorImpl final : public Allocator
{
public:
	static AllocatorImpl& GetInstance(PageAllocator* pgAlloc)
	{
		static AllocatorImpl Instance(pgAlloc);
		return Instance;
	}

	virtual void* Alloc(size_t size, ulong tag) override;
	virtual void Free(void* ptr) override;

	/* Alloc, for a caller that asks for no particular contents: Rust's
	   global allocator, whose alloc promises uninitialised memory and whose
	   alloc_zeroed zeroes it itself. A block of pages the page allocator
	   kept from a free comes back as it was left rather than zeroed --
	   which on arm64, whose memset is a byte loop, is most of what such a
	   block costs. */
	void* AllocUninit(size_t size, ulong tag);

	/* Give back to the page allocator what the heap keeps for its next
	   allocations -- the pools' empty pages, then the freed blocks the page
	   allocator holds mapped; whether there was any. Called without a spin
	   lock held. */
	bool Trim();
private:
	AllocatorImpl(PageAllocator* pgAlloc);
	~AllocatorImpl();

	AllocatorImpl(const AllocatorImpl& other) = delete;
	AllocatorImpl(AllocatorImpl&& other) = delete;
	AllocatorImpl& operator=(const AllocatorImpl& other) = delete;
	AllocatorImpl& operator=(AllocatorImpl&& other) = delete;

	static const u32 Magic = 0xCBDECBDE;

	size_t Log2(size_t size);
	bool LogBySize(size_t size, size_t& log);
	/* zeroPages: whether a block past the pools comes back zeroed */
	void* AllocBlock(size_t size, ulong tag, bool zeroPages);

	struct Header {
		u32 Magic;
		u32 Size;
	};

	static const size_t StartLog = 3;
	static const size_t EndLog = Const::PageShift - 1;
	static constexpr size_t PoolCount = EndLog - StartLog + 1;

	/* Per-CPU caches in front of the pools. A block taken from a pool or
	   given back to it takes the pool's lock, which every CPU allocating
	   that size shares: on four CPUs a pair of them cost twelve times what
	   it does on one (heapbench). A CPU's cache keeps a few free blocks of
	   each size under a lock of its own, which only that CPU takes -- and
	   Trim -- and trades them with the pools half a cache at a time, the
	   pool's lock once a batch. A block in a cache is free: FreedTag, and
	   nobody's. */
	static constexpr size_t CacheBlocksMax = 16;
	/* The most a CPU's cache of one size holds: CacheBlocksMax blocks, and
	   no more bytes than a page -- two of the largest blocks, each of which
	   has a page to itself */
	static constexpr size_t CacheBytes = Const::PageSize;

	struct SizeCache
	{
		size_t Count;
		size_t Limit;
		void* Blocks[CacheBlocksMax];
	};

	struct alignas(64) CpuCache
	{
		SpinLock Lock;
		SizeCache Sizes[PoolCount];
	};

	/* This CPU's cache; nullptr while it cannot yet say which CPU it is */
	CpuCache* ThisCpuCache();
	/* A free block of Pool[pool] from this CPU's cache, which refills from
	   the pool when empty; nullptr when neither has one */
	void* CacheTake(size_t pool);
	/* A freed block of Pool[pool] into this CPU's cache, the older half of
	   which goes back to the pool when full; false when there is no cache
	   to take it */
	bool CachePut(size_t pool, void* block);

	Pool Pool[PoolCount];
	PageAllocator* PgAlloc;
	CpuCache Caches[CacheCpus]; /* cpu_cache.h's */
};

}
}
