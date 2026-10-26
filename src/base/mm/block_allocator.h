#pragma once

#include <kernel/spin_lock.h>
#include <lib/stdlib.h>
#include <lib/list_entry.h>

namespace kernel
{

namespace mm
{

class BlockAllocatorImpl
{
public:
    BlockAllocatorImpl();
    ~BlockAllocatorImpl();

    bool setup(ulong start_address, ulong end_address, ulong block_size);

    void* alloc();

    bool is_owner(void *block);

    void free(void *block);

private:
    BlockAllocatorImpl(const BlockAllocatorImpl& other) = delete;
    BlockAllocatorImpl(BlockAllocatorImpl&& other) = delete;
    BlockAllocatorImpl& operator=(const BlockAllocatorImpl& other) = delete;
    BlockAllocatorImpl& operator=(BlockAllocatorImpl&& other) = delete;

    using ListEntry = stdlib::ListEntry;

    static const ulong MAGIC = 0xbeadbead;

    struct BlockEntry
    {
        ulong magic;
        ListEntry list_link;
        ulong frames[10];
        size_t num_frames;
    };

    ListEntry free_block_list_;
    ListEntry active_block_list_;

    ulong usage_;
    ulong total_;
    ulong start_address_;
    ulong end_address_;
    ulong block_size_;
    SpinLock lock_;
};

}
}