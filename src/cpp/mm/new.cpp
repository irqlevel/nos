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

void* AllocMapPages(size_t numPages, ulong* physAddr)
{
	return PageAllocatorImpl::GetInstance().AllocMapPages(numPages, physAddr);
}

void UnmapFreePages(void* ptr)
{
	PageAllocatorImpl::GetInstance().UnmapFreePages(ptr);
}

void* MapPages(size_t numPages, ulong* physAddrs)
{
	return PageAllocatorImpl::GetInstance().MapPages(numPages, physAddrs);
}

void UnmapPages(void* ptr, size_t numPages)
{
	PageAllocatorImpl::GetInstance().UnmapPages(ptr, numPages);
}

}
}

/* The plain forms must not return nullptr (see NoThrowT in new.h): the
   compiler runs the constructor on the result unchecked, so returning null
   here would mean a write to VA 0 instead of a diagnosable panic. */
void* operator new(size_t size)
{
    void* ptr = Kernel::Mm::Alloc(size, 0);
    if (ptr == nullptr)
        Panic("operator new: out of memory (size %u)", (ulong)size);
    return ptr;
}

void* operator new[](size_t size)
{
    void* ptr = Kernel::Mm::Alloc(size, 0);
    if (ptr == nullptr)
        Panic("operator new[]: out of memory (size %u)", (ulong)size);
    return ptr;
}

void* operator new(size_t size, void *ptr) noexcept
{
    (void)size;
    return ptr;
}

void* operator new[](size_t size, void *ptr) noexcept
{
    (void)size;
    return ptr;
}

void* operator new(size_t size, const Kernel::Mm::NoThrowT&) noexcept
{
    return Kernel::Mm::Alloc(size, 0);
}

void* operator new[](size_t size, const Kernel::Mm::NoThrowT&) noexcept
{
    return Kernel::Mm::Alloc(size, 0);
}

void operator delete(void* ptr, const Kernel::Mm::NoThrowT&) noexcept
{
    Kernel::Mm::Free(ptr);
}

void operator delete[](void* ptr, const Kernel::Mm::NoThrowT&) noexcept
{
    Kernel::Mm::Free(ptr);
}

void operator delete(void* ptr)
{
	Kernel::Mm::Free(ptr);
}

void operator delete[](void* ptr)
{
	Kernel::Mm::Free(ptr);
}
