#pragma once

#include <include/const.h>
#include <kernel/spin_lock.h>

namespace kernel
{

namespace mm
{

class VaAllocator
{
public:
    VaAllocator();
    ~VaAllocator();

    bool setup(ulong start, ulong end, ulong block_size);

    ulong alloc();
    void free(ulong va);
    bool contains(ulong va);

    /* How many blocks it has in all, taken and free */
    ulong block_count() const { return block_count_; }

private:
    VaAllocator(const VaAllocator& other) = delete;
    VaAllocator(VaAllocator&& other) = delete;
    VaAllocator& operator=(const VaAllocator& other) = delete;
    VaAllocator& operator=(VaAllocator&& other) = delete;

    u8* bitmap_ptr_;
    ulong bitmap_size_;
    SpinLock lock_;
    ulong va_start_;
    ulong va_end_;
    ulong block_size_;
    ulong block_count_;
};

}
}
