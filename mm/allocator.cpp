#include "allocator.h"

#include <include/const.h>
#include <kernel/panic.h>
#include <kernel/trace.h>
#include <lib/stdlib.h>

namespace Kernel
{

namespace Mm
{

AllocatorImpl::AllocatorImpl(class PageAllocator* pgAlloc)
	: PgAlloc(pgAlloc)
{
	for (size_t i = 0; i < Stdlib::ArraySize(Pool); i++)
	{
		Pool[i].Init(static_cast<size_t>(1) << (StartLog + i), PgAlloc);
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
	BugOn(size == 0);

	Header* header;
	size_t reqSize = (size + sizeof(*header));
	if (reqSize >= (Const::PageSize / 2))
	{
		return PgAlloc->Alloc(Stdlib::SizeInPages(size));
	}

	size_t log = Log2(reqSize);

	Trace(AllocatorLL, "0x%p size 0x%p log 0x%p", this, size, log);
	if (BugOn(log < StartLog || log > EndLog || (log - StartLog) >= Stdlib::ArraySize(Pool)))
	{
		return nullptr;
	}

	header = static_cast<Header*>(Pool[log - StartLog].Alloc(tag));
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

	Pool[log - StartLog].Free(header);
}

}
}