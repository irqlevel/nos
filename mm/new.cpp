#include "new.h"
#include "page_allocator.h"
#include "allocator.h"

#include <include/const.h>
#include <kernel/panic.h>

namespace Kernel
{
namespace Mm
{

void* Alloc(size_t size, ulong tag)
{
	return AllocatorImpl::GetInstance(&PageAllocatorImpl::GetInstance()).Alloc(size, tag);
}

void Free(void* ptr)
{
	AllocatorImpl::GetInstance(&PageAllocatorImpl::GetInstance()).Free(ptr);
}


}
}

void* operator new(size_t size)
{
    return Kernel::Mm::Alloc(size, 0);
}

void* operator new[](size_t size)
{
    return Kernel::Mm::Alloc(size, 0);
}

void* operator new(size_t size, void *ptr)
{
    (void)size;
    return ptr;
}

void* operator new[](size_t size, void *ptr)
{
    (void)size;
    return ptr;
}

void operator delete(void* ptr)
{
	Kernel::Mm::Free(ptr);
}

void operator delete[](void* ptr)
{
	Kernel::Mm::Free(ptr);
}
