#include "new.h"
#include "page_allocator.h"
#include "allocator.h"

#include <include/const.h>
#include <kernel/panic.h>

namespace kernel
{
namespace mm
{

void* alloc(size_t size, ulong tag)
{
	return AllocatorImpl::get_instance(&PageAllocatorImpl::get_instance()).alloc(size, tag);
}

void* alloc_uninit(size_t size, ulong tag)
{
	return AllocatorImpl::get_instance(&PageAllocatorImpl::get_instance()).alloc_uninit(size, tag);
}

void free(void* ptr)
{
	AllocatorImpl::get_instance(&PageAllocatorImpl::get_instance()).free(ptr);
}

void* alloc_map_pages(size_t num_pages, ulong* phys_addr)
{
	return PageAllocatorImpl::get_instance().alloc_map_pages(num_pages, phys_addr);
}

void unmap_free_pages(void* ptr)
{
	PageAllocatorImpl::get_instance().unmap_free_pages(ptr);
}

void* map_pages(size_t num_pages, ulong* phys_addrs)
{
	return PageAllocatorImpl::get_instance().map_pages(num_pages, phys_addrs);
}

void unmap_pages(void* ptr, size_t num_pages)
{
	PageAllocatorImpl::get_instance().unmap_pages(ptr, num_pages);
}

void* map_large_pages(size_t num_pages, ulong* phys_addrs)
{
	return PageAllocatorImpl::get_instance().map_large_pages(num_pages, phys_addrs);
}

void unmap_large_pages(void* ptr, size_t num_pages)
{
	PageAllocatorImpl::get_instance().unmap_large_pages(ptr, num_pages);
}

}
}

/* The plain forms must not return nullptr (see NoThrowT in new.h): the
   compiler runs the constructor on the result unchecked, so returning null
   here would mean a write to VA 0 instead of a diagnosable panic. */
void* operator new(size_t size)
{
    void* ptr = kernel::mm::alloc(size, 0);
    if (ptr == nullptr)
        PANIC("operator new: out of memory (size %lu)", (ulong)size);
    return ptr;
}

void* operator new[](size_t size)
{
    void* ptr = kernel::mm::alloc(size, 0);
    if (ptr == nullptr)
        PANIC("operator new[]: out of memory (size %lu)", (ulong)size);
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

void* operator new(size_t size, const kernel::mm::NoThrowT&) noexcept
{
    return kernel::mm::alloc(size, 0);
}

void* operator new[](size_t size, const kernel::mm::NoThrowT&) noexcept
{
    return kernel::mm::alloc(size, 0);
}

void operator delete(void* ptr, const kernel::mm::NoThrowT&) noexcept
{
    kernel::mm::free(ptr);
}

void operator delete[](void* ptr, const kernel::mm::NoThrowT&) noexcept
{
    kernel::mm::free(ptr);
}

void operator delete(void* ptr)
{
	kernel::mm::free(ptr);
}

void operator delete[](void* ptr)
{
	kernel::mm::free(ptr);
}
