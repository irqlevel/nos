#include "allocator.h"

#include <include/const.h>
#include <kernel/cpu.h>
#include <kernel/panic.h>
#include <kernel/trace.h>
#include <lib/lock.h>
#include <lib/stdlib.h>

namespace kernel
{

namespace mm
{

AllocatorImpl::AllocatorImpl(class PageAllocator* pg_alloc)
	: pg_alloc_(pg_alloc)
{
	static_assert(CACHE_CPUS == MAX_CPUS, "a CPU without a cache, or a cache without a CPU");
	static_assert(CACHE_BLOCKS_MAX <= mm::Pool::PUT_FREE_MAX, "a cache's spill past what PutFree takes");

	for (size_t i = 0; i < stdlib::array_size(pool_); i++)
	{
		pool_[i].init(static_cast<size_t>(1) << (START_LOG + i), pg_alloc_);
	}

	for (auto& cache : caches_)
	{
		for (size_t i = 0; i < POOL_COUNT; i++)
		{
			cache.sizes[i].count = 0;
			cache.sizes[i].limit = stdlib::min<size_t>(CACHE_BLOCKS_MAX, CACHE_BYTES >> (START_LOG + i));
			BUG_ON(cache.sizes[i].limit < 2);
		}
	}
}

AllocatorImpl::~AllocatorImpl()
{
    trace(0, "0x%p dtor", this);
}

size_t AllocatorImpl::log2(size_t size)
{
	size_t log;

	if (size <= 16)
	{
		log = 4;
	}
	else
	{
		log = stdlib::log2(size);
	}

	BUG_ON((static_cast<size_t>(1) << log) < size);

	return log;
}

void* AllocatorImpl::alloc(size_t size, ulong tag)
{
	return alloc_block(size, tag, true);
}

void* AllocatorImpl::alloc_uninit(size_t size, ulong tag)
{
	return alloc_block(size, tag, false);
}

void* AllocatorImpl::alloc_block(size_t size, ulong tag, bool zero_pages)
{
	BUG_ON(size == 0);

	Header* header;
	if (size > static_cast<size_t>(-1) - sizeof(*header))
		return nullptr;

	size_t req_size = (size + sizeof(*header));
	if (req_size >= (consts::PAGE_SIZE / 2))
	{
		return pg_alloc_->alloc(stdlib::size_in_pages(size), zero_pages);
	}

	size_t log = log2(req_size);

	trace(ALLOCATOR_LL, "0x%p size 0x%lX log 0x%lX", this, size, log);
	if (BUG_ON(log < START_LOG || log > END_LOG || (log - START_LOG) >= stdlib::array_size(pool_)))
	{
		return nullptr;
	}

	size_t pool = log - START_LOG;
	header = static_cast<Header*>(cache_take(pool));
	if (header != nullptr)
		pool_[pool].claim(header, tag);
	else
		header = static_cast<Header*>(pool_[pool].alloc(tag));
	if (header == nullptr)
	{
		return nullptr;
	}

	header->magic = MAGIC;
	header->size = size;
	return header + 1;
}

void AllocatorImpl::free(void* ptr)
{
	BUG_ON(ptr == nullptr);

	if ((reinterpret_cast<ulong>(ptr) & (consts::PAGE_SIZE - 1)) == 0)
	{
		pg_alloc_->free(ptr);
		return;
	}

	Header *header = static_cast<Header*>(ptr) - 1;
	if (header->magic != MAGIC)
	{
		PANIC("Invalid header magic");
		return;
	}

	size_t log = log2(header->size + sizeof(*header));
	if (BUG_ON(log < START_LOG || log > END_LOG || (log - START_LOG) >= stdlib::array_size(pool_)))
	{
		return;
	}

	size_t pool = log - START_LOG;
	pool_[pool].mark_freed(header);
	if (!cache_put(pool, header))
	{
		void* block = header;
		pool_[pool].put_free(&block, 1);
	}
}

AllocatorImpl::CpuCache* AllocatorImpl::this_cpu_cache()
{
	ulong cpu;
	if (!cache_cpu(cpu))
		return nullptr;
	return &caches_[cpu];
}

/* The pool's lock is taken inside the cache's, never the other way round:
   a pool does not reach the caches. Neither is held where a page is
   allocated or given back. */
void* AllocatorImpl::cache_take(size_t pool)
{
	CpuCache* cache = this_cpu_cache();
	if (cache == nullptr)
		return nullptr;

	stdlib::AutoLock lock(cache->lock);
	SizeCache& size = cache->sizes[pool];
	/* Refilled to half: the other half is room for the frees to come */
	if (size.count == 0)
		size.count = pool_[pool].take_free(size.blocks, size.limit / 2);
	if (size.count == 0)
		return nullptr;
	return size.blocks[--size.count];
}

bool AllocatorImpl::cache_put(size_t pool, void* block)
{
	CpuCache* cache = this_cpu_cache();
	if (cache == nullptr)
		return false;

	void* spill[CACHE_BLOCKS_MAX];
	size_t spilled = 0;
	{
		stdlib::AutoLock lock(cache->lock);
		SizeCache& size = cache->sizes[pool];
		if (size.count >= size.limit)
		{
			/* The older half: the newer is likelier still in this CPU's
			   data cache */
			spilled = size.limit / 2;
			for (size_t i = 0; i < spilled; i++)
				spill[i] = size.blocks[i];
			for (size_t i = spilled; i < size.count; i++)
				size.blocks[i - spilled] = size.blocks[i];
			size.count -= spilled;
		}
		size.blocks[size.count++] = block;
	}

	/* With the cache's lock let go of: a page the pool gives back may be
	   unmapped, and the shootdown waits on every other CPU */
	if (spilled != 0)
		pool_[pool].put_free(spill, spilled);
	return true;
}

bool AllocatorImpl::trim()
{
	bool trimmed = false;
	/* Every CPU's cache into its pools first, so that their pages can go */
	for (auto& cache : caches_)
	{
		for (size_t pool = 0; pool < POOL_COUNT; pool++)
		{
			void* blocks[CACHE_BLOCKS_MAX];
			size_t count;
			{
				stdlib::AutoLock lock(cache.lock);
				SizeCache& size = cache.sizes[pool];
				count = size.count;
				for (size_t i = 0; i < count; i++)
					blocks[i] = size.blocks[i];
				size.count = 0;
			}
			if (count != 0)
			{
				pool_[pool].put_free(blocks, count);
				trimmed = true;
			}
		}
	}

	/* The pools next: the pages they give back are kept by the page
	   allocator, until its own trim below */
	for (size_t i = 0; i < stdlib::array_size(pool_); i++)
	{
		if (pool_[i].trim())
			trimmed = true;
	}
	if (pg_alloc_->trim())
		trimmed = true;
	return trimmed;
}

}
}