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

namespace fuzz
{

namespace
{

/* The largest block the kernel's heap hands out; past it, nullptr. */
const size_t MAX_BLOCK = 64 << 20;

size_t blocks;
size_t bytes;
long fail_after = -1;

/* Each live block's size, to count what Free gives back. */
std::map<void*, size_t>& live()
{
    static std::map<void*, size_t> live;
    return live;
}

}

size_t heap_blocks()
{
    return blocks;
}

size_t heap_bytes()
{
    return bytes;
}

void heap_fail_after(long n)
{
    fail_after = n;
}

void heap_reset()
{
    fail_after = -1;
}

static void* heap_alloc(size_t size)
{
    INVARIANT(held_spin_locks() == 0, "an allocation with %ld spin locks held", held_spin_locks());
    if (fail_after == 0)
        return nullptr;
    if (fail_after > 0)
        fail_after--;
    if (size == 0 || size > MAX_BLOCK)
        return nullptr;
    void* p = malloc(size);
    if (p == nullptr)
        return nullptr;
    live()[p] = size;
    blocks++;
    bytes += size;
    return p;
}

static void heap_free(void* p)
{
    if (p == nullptr)
        return;
    INVARIANT(held_spin_locks() == 0, "a free with %ld spin locks held", held_spin_locks());
    auto it = live().find(p);
    INVARIANT(it != live().end(), "a free of %p, which is no block of the kernel heap", p);
    blocks--;
    bytes -= it->second;
    live().erase(it);
    free(p);
}

}

namespace kernel
{
namespace mm
{

void* alloc(size_t size, ulong tag)
{
    (void)tag;
    return fuzz::heap_alloc(size);
}

void free(void* ptr)
{
    fuzz::heap_free(ptr);
}

}
}

void* operator new(size_t size, const kernel::mm::NoThrowT&) noexcept
{
    return fuzz::heap_alloc(size);
}

void* operator new[](size_t size, const kernel::mm::NoThrowT&) noexcept
{
    return fuzz::heap_alloc(size);
}

void operator delete(void* ptr, const kernel::mm::NoThrowT&) noexcept
{
    fuzz::heap_free(ptr);
}

void operator delete[](void* ptr, const kernel::mm::NoThrowT&) noexcept
{
    fuzz::heap_free(ptr);
}
