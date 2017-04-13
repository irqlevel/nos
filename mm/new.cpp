#include "new.h"
#include "page_allocator.h"
#include "allocator.h"

#include <include/const.h>
#include <kernel/panic.h>

namespace Kernel
{

namespace Core
{

void* New(size_t size) noexcept
{

	return AllocatorImpl::GetInstance(PageAllocatorImpl::GetInstance()).Alloc(size);
}

void Delete(void* ptr) noexcept
{
	AllocatorImpl::GetInstance(PageAllocatorImpl::GetInstance()).Free(ptr);
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
