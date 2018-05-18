#pragma once

#include <include/types.h>
#include <lib/stdlib.h>

namespace Kernel
{

namespace Mm
{

void* Alloc(size_t size, ulong tag);

void Free(void* ptr);

template<typename T, ulong tag, class... Args>
T* TAlloc(Args&&... args)
{
    void* p = Alloc(sizeof(T), tag);
    if (p == nullptr)
        return nullptr;

    return new (p) T(Stdlib::Forward<Args>(args)...);
}

}
}

void* operator new(size_t size);
void* operator new[](size_t size);

void* operator new(size_t size, void *ptr);
void* operator new[](size_t size, void *ptr);

void operator delete(void* ptr);
void operator delete[](void* ptr);
