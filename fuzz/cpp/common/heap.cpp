// The kernel heap, for a target that does not test the kernel's own: blocks
// from the host's allocator, each counted, so a target can see what an error
// path failed to give back, and each allocation able to fail on the target's
// word, so the error paths are taken. Mm::Alloc of a size the kernel's heap
// would refuse returns nullptr, as the kernel's does. Allocating or freeing
// with a spin lock held is a finding: the kernel's allocator may shoot down
// every CPU's TLB and wait for them, and a CPU spinning on that lock never
// answers (CLAUDE.md).
#include "host.h"

#include <mm/new.h>

#include "fuzz.h"
#include "kernel.h"

#include <stdlib.h>

#include <map>

namespace Fuzz
{

namespace
{

/* The largest block the kernel's heap hands out; past it, nullptr. */
const size_t MaxBlock = 64 << 20;

size_t Blocks;
size_t Bytes;
long FailAfter = -1;

/* Each live block's size, to count what Free gives back. */
std::map<void*, size_t>& Live()
{
    static std::map<void*, size_t> live;
    return live;
}

}

size_t HeapBlocks()
{
    return Blocks;
}

size_t HeapBytes()
{
    return Bytes;
}

void HeapFailAfter(long n)
{
    FailAfter = n;
}

void HeapReset()
{
    FailAfter = -1;
}

static void* HeapAlloc(size_t size)
{
    INVARIANT(HeldSpinLocks() == 0, "an allocation with %ld spin locks held", HeldSpinLocks());
    if (FailAfter == 0)
        return nullptr;
    if (FailAfter > 0)
        FailAfter--;
    if (size == 0 || size > MaxBlock)
        return nullptr;
    void* p = malloc(size);
    if (p == nullptr)
        return nullptr;
    Live()[p] = size;
    Blocks++;
    Bytes += size;
    return p;
}

static void HeapFree(void* p)
{
    if (p == nullptr)
        return;
    INVARIANT(HeldSpinLocks() == 0, "a free with %ld spin locks held", HeldSpinLocks());
    auto it = Live().find(p);
    INVARIANT(it != Live().end(), "a free of %p, which is no block of the kernel heap", p);
    Blocks--;
    Bytes -= it->second;
    Live().erase(it);
    free(p);
}

}

namespace Kernel
{
namespace Mm
{

void* Alloc(size_t size, ulong tag)
{
    (void)tag;
    return Fuzz::HeapAlloc(size);
}

void Free(void* ptr)
{
    Fuzz::HeapFree(ptr);
}

}
}

void* operator new(size_t size, const Kernel::Mm::NoThrowT&) noexcept
{
    return Fuzz::HeapAlloc(size);
}

void* operator new[](size_t size, const Kernel::Mm::NoThrowT&) noexcept
{
    return Fuzz::HeapAlloc(size);
}

void operator delete(void* ptr, const Kernel::Mm::NoThrowT&) noexcept
{
    Fuzz::HeapFree(ptr);
}

void operator delete[](void* ptr, const Kernel::Mm::NoThrowT&) noexcept
{
    Fuzz::HeapFree(ptr);
}
