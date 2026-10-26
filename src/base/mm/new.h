#pragma once

#include <include/types.h>
#include <lib/stdlib.h>

namespace kernel
{

namespace mm
{

void* alloc(size_t size, ulong tag);

/* Alloc with no promise about what the block holds: for Rust's global
   allocator (AllocatorImpl::alloc_uninit) */
void* alloc_uninit(size_t size, ulong tag);

void free(void* ptr);

void* alloc_map_pages(size_t num_pages, ulong* phys_addr);

void unmap_free_pages(void* ptr);

void* map_pages(size_t num_pages, ulong* phys_addrs);

void unmap_pages(void* ptr, size_t num_pages);

/* map_pages for more pages than its largest block holds, up to
   PageTable::MAX_LARGE_MAP_PAGES: a loadable module's image, the .ko it is
   read from. The pages stay the caller's, as with map_pages. */
void* map_large_pages(size_t num_pages, ulong* phys_addrs);

void unmap_large_pages(void* ptr, size_t num_pages);

template<typename T, ulong TAG, class... Args>
T* talloc(Args&&... args)
{
    void* p = alloc(sizeof(T), TAG);
    if (p == nullptr)
        return nullptr;

    return new (p) T(stdlib::forward<Args>(args)...);
}

struct FreeDeleter
{
    void operator()(void* ptr) const { free(ptr); }
};

/* Tag for the non-throwing operator new forms. The replaceable plain forms
   are implicitly declared potentially-throwing (the compiler rejects a
   noexcept redeclaration), so the compiler assumes they never return
   nullptr: constructors run on the result unchecked and callers' nullptr
   tests may be optimized away. Plain new therefore panics on OOM; any
   allocation that wants to observe OOM must use

       T* p = new (mm::NO_THROW) T(...);

   for which the compiler is required to emit a null check before the
   constructor and to preserve callers' tests. */
struct NoThrowT {};
inline constexpr NoThrowT NO_THROW{};

}
}

/* Plain forms: never return nullptr -- panic on OOM (see NoThrowT above). */
void* operator new(size_t size);
void* operator new[](size_t size);

void* operator new(size_t size, void *ptr) noexcept;
void* operator new[](size_t size, void *ptr) noexcept;

/* Fallible forms: return nullptr on OOM. */
void* operator new(size_t size, const kernel::mm::NoThrowT&) noexcept;
void* operator new[](size_t size, const kernel::mm::NoThrowT&) noexcept;

/* Matching placement deletes: only reachable if a constructor throws,
   impossible with -fno-exceptions; declared for completeness. */
void operator delete(void* ptr, const kernel::mm::NoThrowT&) noexcept;
void operator delete[](void* ptr, const kernel::mm::NoThrowT&) noexcept;

void operator delete(void* ptr);
void operator delete[](void* ptr);
