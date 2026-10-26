#pragma once

#include "cpu_cache.h"
#include "page_allocator.h"
#include "pool.h"

#include <include/const.h>
#include <kernel/spin_lock.h>
#include <lib/list_entry.h>

namespace kernel
{

namespace mm
{

class Allocator
{
public:
	virtual void* alloc(size_t size, ulong tag) = 0;
	virtual void free(void* ptr) = 0;
};

/* final, with a destructor that is not virtual: the per-CPU caches below are
   aligned to cache lines, which makes the class over-aligned, and a virtual
   destructor of an over-aligned class calls the aligned operator delete the
   kernel does not have. The one instance is a static nobody deletes. */
class AllocatorImpl final : public Allocator
{
public:
	static AllocatorImpl& get_instance(PageAllocator* pg_alloc)
	{
		static AllocatorImpl instance(pg_alloc);
		return instance;
	}

	virtual void* alloc(size_t size, ulong tag) override;
	virtual void free(void* ptr) override;

	/* Alloc, for a caller that asks for no particular contents: Rust's
	   global allocator, whose alloc promises uninitialised memory and whose
	   alloc_zeroed zeroes it itself. A block of pages the page allocator
	   kept from a free comes back as it was left rather than zeroed --
	   which on arm64, whose memset is a byte loop, is most of what such a
	   block costs. */
	void* alloc_uninit(size_t size, ulong tag);

	/* Give back to the page allocator what the heap keeps for its next
	   allocations -- the pools' empty pages, then the freed blocks the page
	   allocator holds mapped; whether there was any. Called without a spin
	   lock held. */
	bool trim();
private:
	AllocatorImpl(PageAllocator* pg_alloc);
	~AllocatorImpl();

	AllocatorImpl(const AllocatorImpl& other) = delete;
	AllocatorImpl(AllocatorImpl&& other) = delete;
	AllocatorImpl& operator=(const AllocatorImpl& other) = delete;
	AllocatorImpl& operator=(AllocatorImpl&& other) = delete;

	static const u32 MAGIC = 0xCBDECBDE;

	size_t log2(size_t size);
	bool log_by_size(size_t size, size_t& log);
	/* zero_pages: whether a block past the pools comes back zeroed */
	void* alloc_block(size_t size, ulong tag, bool zero_pages);

	struct Header {
		u32 magic;
		u32 size;
	};

	static const size_t START_LOG = 3;
	static const size_t END_LOG = consts::PAGE_SHIFT - 1;
	static constexpr size_t POOL_COUNT = END_LOG - START_LOG + 1;

	/* Per-CPU caches in front of the pools. A block taken from a pool or
	   given back to it takes the pool's lock, which every CPU allocating
	   that size shares: on four CPUs a pair of them cost twelve times what
	   it does on one (heapbench). A CPU's cache keeps a few free blocks of
	   each size under a lock of its own, which only that CPU takes -- and
	   Trim -- and trades them with the pools half a cache at a time, the
	   pool's lock once a batch. A block in a cache is free: FREED_TAG, and
	   nobody's. */
	static constexpr size_t CACHE_BLOCKS_MAX = 16;
	/* The most a CPU's cache of one size holds: CACHE_BLOCKS_MAX blocks, and
	   no more bytes than a page -- two of the largest blocks, each of which
	   has a page to itself */
	static constexpr size_t CACHE_BYTES = consts::PAGE_SIZE;

	struct SizeCache
	{
		size_t count;
		size_t limit;
		void* blocks[CACHE_BLOCKS_MAX];
	};

	struct alignas(64) CpuCache
	{
		SpinLock lock;
		SizeCache sizes[POOL_COUNT];
	};

	/* This CPU's cache; nullptr while it cannot yet say which CPU it is */
	CpuCache* this_cpu_cache();
	/* A free block of Pool[pool] from this CPU's cache, which refills from
	   the pool when empty; nullptr when neither has one */
	void* cache_take(size_t pool);
	/* A freed block of Pool[pool] into this CPU's cache, the older half of
	   which goes back to the pool when full; false when there is no cache
	   to take it */
	bool cache_put(size_t pool, void* block);

	Pool pool_[POOL_COUNT];
	PageAllocator* pg_alloc_;
	CpuCache caches_[CACHE_CPUS]; /* cpu_cache.h's */
};

}
}
