#pragma once

#include <include/types.h>
#include <lib/stdlib.h>

namespace Kernel
{

namespace Mm
{

void* Alloc(size_t size, ulong tag);

void Free(void* ptr);

void* AllocMapPages(size_t numPages, ulong* physAddr);

void UnmapFreePages(void* ptr);

void* MapPages(size_t numPages, ulong* physAddrs);

void UnmapPages(void* ptr, size_t numPages);

template<typename T, ulong tag, class... Args>
T* TAlloc(Args&&... args)
{
    void* p = Alloc(sizeof(T), tag);
    if (p == nullptr)
        return nullptr;

    return new (p) T(Stdlib::Forward<Args>(args)...);
}

struct FreeDeleter
{
    void operator()(void* ptr) const { Free(ptr); }
};

/* Tag for the non-throwing operator new forms. The replaceable plain forms
   are implicitly declared potentially-throwing (the compiler rejects a
   noexcept redeclaration), so the compiler assumes they never return
   nullptr: constructors run on the result unchecked and callers' nullptr
   tests may be optimized away. Plain new therefore panics on OOM; any
   allocation that wants to observe OOM must use

       T* p = new (Mm::NoThrow) T(...);

   for which the compiler is required to emit a null check before the
   constructor and to preserve callers' tests. */
struct NoThrowT {};
inline constexpr NoThrowT NoThrow{};

}
}

/* Plain forms: never return nullptr -- panic on OOM (see NoThrowT above). */
void* operator new(size_t size);
void* operator new[](size_t size);

void* operator new(size_t size, void *ptr) noexcept;
void* operator new[](size_t size, void *ptr) noexcept;

/* Fallible forms: return nullptr on OOM. */
void* operator new(size_t size, const Kernel::Mm::NoThrowT&) noexcept;
void* operator new[](size_t size, const Kernel::Mm::NoThrowT&) noexcept;

/* Matching placement deletes: only reachable if a constructor throws,
   impossible with -fno-exceptions; declared for completeness. */
void operator delete(void* ptr, const Kernel::Mm::NoThrowT&) noexcept;
void operator delete[](void* ptr, const Kernel::Mm::NoThrowT&) noexcept;

void operator delete(void* ptr);
void operator delete[](void* ptr);
