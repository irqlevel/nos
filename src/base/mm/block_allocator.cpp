#include "block_allocator.h"

#include <kernel/trace.h>
#include <kernel/panic.h>
#include <kernel/stack_trace.h>

namespace kernel
{

namespace mm
{

BlockAllocatorImpl::BlockAllocatorImpl()
    : usage_(0)
    , total_(0)
    , start_address_(0)
    , end_address_(0)
    , block_size_(0)
{
    stdlib::AutoLock lock(lock_);

    free_block_list_.init();
    active_block_list_.init();
}

bool BlockAllocatorImpl::setup(ulong start_address, ulong end_address, ulong block_size)
{
    trace(0, "0x%p start 0x%lX end 0x%lX bsize %lu", this, start_address, end_address, block_size);

    stdlib::AutoLock lock(lock_);
    if (total_ != 0)
        return false;

    if (end_address <= start_address)
        return false;

    if (sizeof(BlockEntry) > block_size)
        return false;

    block_size_ = block_size;
    start_address_ = end_address_ = start_address;
    for (ulong block_address = start_address; (block_address + 2 * block_size_) <= end_address;
        block_address+= (2 * block_size_))
    {
        BlockEntry* b = reinterpret_cast<BlockEntry*>(block_address + block_size_);
        b->magic = MAGIC;
        b->num_frames = 0;
        free_block_list_.insert_tail(&b->list_link);
        end_address_ = block_address + 2 * block_size_;
        total_++;
    }

    trace(0, "0x%p start 0x%lX end 0x%lX bsize %lu total %lu", this, start_address_, end_address_, block_size_, total_);

    return (total_ != 0) ? true : false;
}

BlockAllocatorImpl::~BlockAllocatorImpl()
{
    stdlib::AutoLock lock(lock_);

    if (usage_ != 0)
    {
        trace(0, "0x%p usage %lu block_size %lu", this, usage_, block_size_);
        for (ListEntry* le = active_block_list_.flink; le != &active_block_list_; le = le->flink)
        {
            BlockEntry* b = CONTAINING_RECORD(le, BlockEntry, list_link);
            trace(0, "leak entry 0x%p magic 0x%lX frames %lu", b, b->magic, b->num_frames);

            for (size_t i = 0; i < b->num_frames; i++)
                trace(0, "leak entry 0x%p stack[%lu]=0x%lX", b, i, b->frames[i]);
        }
        trace(0, "0x%p usage %lu block_size %lu", this, usage_, block_size_);
    }

    BUG_ON(usage_ != 0);
}

void* BlockAllocatorImpl::alloc()
{
    stdlib::AutoLock lock(lock_);

    if (free_block_list_.is_empty())
    {
        return nullptr;
    }

    usage_++;
    BlockEntry* b = CONTAINING_RECORD(free_block_list_.remove_head(), BlockEntry, list_link);
    BUG_ON(b->magic != MAGIC);
    b->num_frames = StackTrace::capture(b->frames, stdlib::array_size(b->frames));
    active_block_list_.insert_head(&b->list_link);

    return reinterpret_cast<void*>(reinterpret_cast<ulong>(b) - block_size_);
}

bool BlockAllocatorImpl::is_owner(void *block)
{
    return stdlib::is_value_in_range((ulong)block, start_address_, end_address_);
}

void BlockAllocatorImpl::free(void *block)
{
    BUG_ON(block == nullptr);
    BUG_ON(!is_owner(block));
    BlockEntry *b = reinterpret_cast<BlockEntry *>(reinterpret_cast<ulong>(block) + block_size_);
    BUG_ON(b->magic != MAGIC);

    stdlib::AutoLock lock(lock_);
    b->list_link.remove();
    b->num_frames = 0;
    free_block_list_.insert_tail(&b->list_link);
    usage_--;
}

}
}
