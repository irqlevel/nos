#include "sallocator.h"
#include "stdlib.h"
#include "panic.h"
#include "const.h"
#include "trace.h"

namespace Kernel
{

namespace Core
{

SAllocator::SAllocator(PageAllocator& pageAllocator)
	: PageAllocer(pageAllocator)
{
	for (size_t i = 0; i < Shared::ArraySize(Pool); i++)
	{
		Pool[i].Reset(static_cast<size_t>(1) << (StartLog + i), &PageAllocer);
	}
}

SAllocator::~SAllocator()
{
}

size_t SAllocator::Log2(size_t size)
{
	size_t log = 0;

	while (size != 0)
	{
		size >>= 1;
		log++;
	}

	return log;
}

bool SAllocator::LogBySize(size_t size, size_t& log)
{
	Header *header;
	size_t reqSize = size + sizeof(*header);
	size_t candLog = Log2(reqSize);
	if ((static_cast<size_t>(1) << candLog) < reqSize)
	{
		return false;
	}

	if (candLog > EndLog)
	{
		return false;
	}

	if (candLog < StartLog)
		candLog = StartLog;

	log = candLog;
	return true;
}

void* SAllocator::Alloc(size_t size)
{
	if (size == 0 || size > PAGE_SIZE)
	{
		return nullptr;
	}

	if (size == PAGE_SIZE)
	{
		return PageAllocer.Alloc();
	}

	size_t log;
	if (!LogBySize(size, log))
	{
		if (size < PAGE_SIZE)
		{
			return PageAllocer.Alloc();
		}

		return nullptr;
	}

	Header* header = static_cast<Header*>(Pool[log].Alloc());
	if (header == nullptr)
	{
		return nullptr;
	}

	header->Magic = Magic;
	header->Size = size;
	return header + 1;
}

void SAllocator::Free(void* ptr)
{
	BugOn(ptr == nullptr);

	if ((reinterpret_cast<ulong>(ptr) & (PAGE_SIZE - 1)) == 0)
	{
		PageAllocer.Free(ptr);
		return;
	}

	Header *header = static_cast<Header*>(ptr) - 1;
	if (header->Magic != Magic)
	{
		Panic("Invalid header magic");
		return;
	}

	size_t log;
	if (!LogBySize(header->Size, log))
	{
		Panic("Invalid header");
		return;
	}

	Pool[log].Free(header);
}

}
}