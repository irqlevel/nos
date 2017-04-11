#include "new.h"
#include "spage_allocator.h"
#include "sallocator.h"

#include <include/const.h>
#include <kernel/panic.h>

namespace Kernel
{

namespace Core
{

void* New(size_t size) noexcept
{

	return SAllocator::GetInstance(SPageAllocator::GetInstance()).Alloc(size);
}

void Delete(void* ptr) noexcept
{

	SAllocator::GetInstance(SPageAllocator::GetInstance()).Free(ptr);
}

}
}

void* operator new(size_t size) noexcept
{
	return Kernel::Core::New(size);
}

void* operator new[](size_t size) noexcept
{
	return Kernel::Core::New(size);
}

void operator delete(void* ptr) noexcept
{
	Kernel::Core::Delete(ptr);
}

void operator delete[](void* ptr) noexcept
{
	Kernel::Core::Delete(ptr);
}
