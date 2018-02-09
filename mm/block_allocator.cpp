#include "block_allocator.h"

#include <kernel/trace.h>
#include <kernel/panic.h>
#include <kernel/stack_trace.h>

namespace Kernel
{

namespace Mm
{

BlockAllocatorImpl::BlockAllocatorImpl()
    : Usage(0)
    , Total(0)
    , StartAddress(0)
    , EndAddress(0)
    , BlockSize(0)
{
    Stdlib::AutoLock lock(Lock);

    FreeBlockList.Init();
    ActiveBlockList.Init();
}

bool BlockAllocatorImpl::Setup(ulong startAddress, ulong endAddress, ulong blockSize)
{
    Trace(0, "0x%p start 0x%p end 0x%p bsize %u", this, startAddress, endAddress, blockSize);

    Stdlib::AutoLock lock(Lock);
    if (Total != 0)
        return false;

    if (endAddress <= startAddress)
        return false;

    if (sizeof(BlockEntry) > blockSize)
        return false;

    BlockSize = blockSize;
    StartAddress = EndAddress = startAddress;
    for (ulong blockAddress = startAddress; (blockAddress + 2 * BlockSize) <= endAddress;
        blockAddress+= (2 * BlockSize))
    {
        BlockEntry* b = reinterpret_cast<BlockEntry*>(blockAddress + BlockSize);
        b->Magic = Magic;
        b->NumFrames = 0;
        FreeBlockList.InsertTail(&b->ListLink);
        EndAddress = blockAddress + 2 * BlockSize;
        Total++;
    }

    Trace(0, "0x%p start 0x%p end 0x%p bsize %u total %u", this, StartAddress, EndAddress, BlockSize, Total);

    return (Total != 0) ? true : false;
}

BlockAllocatorImpl::~BlockAllocatorImpl()
{
    Stdlib::AutoLock lock(Lock);

    if (Usage != 0)
    {
        Trace(0, "0x%p usage %u blockSize %u", this, Usage, BlockSize);
        for (ListEntry* le = ActiveBlockList.Flink; le != &ActiveBlockList; le = le->Flink)
        {
            BlockEntry* b = CONTAINING_RECORD(le, BlockEntry, ListLink);
            Trace(0, "leak entry 0x%p magic 0x%p frames %u", b, b->Magic, b->NumFrames);

            for (size_t i = 0; i < b->NumFrames; i++)
                Trace(0, "leak entry 0x%p stack[%u]=0x%p", b, i, b->Frames[i]);
        }
        Trace(0, "0x%p usage %u blockSize %u", this, Usage, BlockSize);
    }

    BugOn(Usage != 0);
}

void* BlockAllocatorImpl::Alloc()
{
    Stdlib::AutoLock lock(Lock);

    if (FreeBlockList.IsEmpty())
    {
        return nullptr;
    }

    Usage++;
    BlockEntry* b = CONTAINING_RECORD(FreeBlockList.RemoveHead(), BlockEntry, ListLink);
    BugOn(b->Magic != Magic);
    b->NumFrames = StackTrace::Capture(4096, b->Frames, Stdlib::ArraySize(b->Frames));
    ActiveBlockList.InsertHead(&b->ListLink);

    return reinterpret_cast<void*>(reinterpret_cast<ulong>(b) - BlockSize);
}

bool BlockAllocatorImpl::IsOwner(void *block)
{
    return Stdlib::IsValueInRange((ulong)block, StartAddress, EndAddress);
}

void BlockAllocatorImpl::Free(void *block)
{
    BugOn(block == nullptr);
    BugOn(!IsOwner(block));
    BlockEntry *b = reinterpret_cast<BlockEntry *>(reinterpret_cast<ulong>(block) + BlockSize);
    BugOn(b->Magic != Magic);

    Stdlib::AutoLock lock(Lock);
    b->ListLink.Remove();
    b->NumFrames = 0;
    FreeBlockList.InsertTail(&b->ListLink);
    Usage--;
}

}
}
