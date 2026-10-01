#include "allocator.h"

#include <include/const.h>
#include <kernel/cpu.h>
#include <kernel/panic.h>
#include <kernel/trace.h>
#include <lib/lock.h>
#include <lib/stdlib.h>

namespace Kernel
{

namespace Mm
{

AllocatorImpl::AllocatorImpl(class PageAllocator* pgAlloc)
	: PgAlloc(pgAlloc)
{
	static_assert(CacheCpus == MaxCpus, "a CPU without a cache, or a cache without a CPU");
	static_assert(CacheBlocksMax <= Mm::Pool::PutFreeMax, "a cache's spill past what PutFree takes");

	for (size_t i = 0; i < Stdlib::ArraySize(Pool); i++)
	{
		Pool[i].Init(static_cast<size_t>(1) << (StartLog + i), PgAlloc);
	}

	for (auto& cache : Caches)
	{
		for (size_t i = 0; i < PoolCount; i++)
		{
			cache.Sizes[i].Count = 0;
			cache.Sizes[i].Limit = Stdlib::Min<size_t>(CacheBlocksMax, CacheBytes >> (StartLog + i));
			BugOn(cache.Sizes[i].Limit < 2);
		}
	}
}

AllocatorImpl::~AllocatorImpl()
{
    Trace(0, "0x%p dtor", this);
}

size_t AllocatorImpl::Log2(size_t size)
{
	size_t log;

	if (size <= 16)
	{
		log = 4;
	}
	else
	{
		log = Stdlib::Log2(size);
	}

	BugOn((static_cast<size_t>(1) << log) < size);

	return log;
}

void* AllocatorImpl::Alloc(size_t size, ulong tag)
{
	return AllocBlock(size, tag, true);
}

void* AllocatorImpl::AllocUninit(size_t size, ulong tag)
{
	return AllocBlock(size, tag, false);
}

void* AllocatorImpl::AllocBlock(size_t size, ulong tag, bool zeroPages)
{
	BugOn(size == 0);

	Header* header;
	if (size > static_cast<size_t>(-1) - sizeof(*header))
		return nullptr;

	size_t reqSize = (size + sizeof(*header));
	if (reqSize >= (Const::PageSize / 2))
	{
		return PgAlloc->Alloc(Stdlib::SizeInPages(size), zeroPages);
	}

	size_t log = Log2(reqSize);

	Trace(AllocatorLL, "0x%p size 0x%lX log 0x%lX", this, size, log);
	if (BugOn(log < StartLog || log > EndLog || (log - StartLog) >= Stdlib::ArraySize(Pool)))
	{
		return nullptr;
	}

	size_t pool = log - StartLog;
	header = static_cast<Header*>(CacheTake(pool));
	if (header != nullptr)
		Pool[pool].Claim(header, tag);
	else
		header = static_cast<Header*>(Pool[pool].Alloc(tag));
	if (header == nullptr)
	{
		return nullptr;
	}

	header->Magic = Magic;
	header->Size = size;
	return header + 1;
}

void AllocatorImpl::Free(void* ptr)
{
	BugOn(ptr == nullptr);

	if ((reinterpret_cast<ulong>(ptr) & (Const::PageSize - 1)) == 0)
	{
		PgAlloc->Free(ptr);
		return;
	}

	Header *header = static_cast<Header*>(ptr) - 1;
	if (header->Magic != Magic)
	{
		Panic("Invalid header magic");
		return;
	}

	size_t log = Log2(header->Size + sizeof(*header));
	if (BugOn(log < StartLog || log > EndLog || (log - StartLog) >= Stdlib::ArraySize(Pool)))
	{
		return;
	}

	size_t pool = log - StartLog;
	Pool[pool].MarkFreed(header);
	if (!CachePut(pool, header))
	{
		void* block = header;
		Pool[pool].PutFree(&block, 1);
	}
}

AllocatorImpl::CpuCache* AllocatorImpl::ThisCpuCache()
{
	ulong cpu;
	if (!CacheCpu(cpu))
		return nullptr;
	return &Caches[cpu];
}

/* The pool's lock is taken inside the cache's, never the other way round:
   a pool does not reach the caches. Neither is held where a page is
   allocated or given back. */
void* AllocatorImpl::CacheTake(size_t pool)
{
	CpuCache* cache = ThisCpuCache();
	if (cache == nullptr)
		return nullptr;

	Stdlib::AutoLock lock(cache->Lock);
	SizeCache& size = cache->Sizes[pool];
	/* Refilled to half: the other half is room for the frees to come */
	if (size.Count == 0)
		size.Count = Pool[pool].TakeFree(size.Blocks, size.Limit / 2);
	if (size.Count == 0)
		return nullptr;
	return size.Blocks[--size.Count];
}

bool AllocatorImpl::CachePut(size_t pool, void* block)
{
	CpuCache* cache = ThisCpuCache();
	if (cache == nullptr)
		return false;

	void* spill[CacheBlocksMax];
	size_t spilled = 0;
	{
		Stdlib::AutoLock lock(cache->Lock);
		SizeCache& size = cache->Sizes[pool];
		if (size.Count >= size.Limit)
		{
			/* The older half: the newer is likelier still in this CPU's
			   data cache */
			spilled = size.Limit / 2;
			for (size_t i = 0; i < spilled; i++)
				spill[i] = size.Blocks[i];
			for (size_t i = spilled; i < size.Count; i++)
				size.Blocks[i - spilled] = size.Blocks[i];
			size.Count -= spilled;
		}
		size.Blocks[size.Count++] = block;
	}

	/* With the cache's lock let go of: a page the pool gives back may be
	   unmapped, and the shootdown waits on every other CPU */
	if (spilled != 0)
		Pool[pool].PutFree(spill, spilled);
	return true;
}

bool AllocatorImpl::Trim()
{
	bool trimmed = false;
	/* Every CPU's cache into its pools first, so that their pages can go */
	for (auto& cache : Caches)
	{
		for (size_t pool = 0; pool < PoolCount; pool++)
		{
			void* blocks[CacheBlocksMax];
			size_t count;
			{
				Stdlib::AutoLock lock(cache.Lock);
				SizeCache& size = cache.Sizes[pool];
				count = size.Count;
				for (size_t i = 0; i < count; i++)
					blocks[i] = size.Blocks[i];
				size.Count = 0;
			}
			if (count != 0)
			{
				Pool[pool].PutFree(blocks, count);
				trimmed = true;
			}
		}
	}

	/* The pools next: the pages they give back are kept by the page
	   allocator, until its own trim below */
	for (size_t i = 0; i < Stdlib::ArraySize(Pool); i++)
	{
		if (Pool[i].Trim())
			trimmed = true;
	}
	if (PgAlloc->Trim())
		trimmed = true;
	return trimmed;
}

}
}