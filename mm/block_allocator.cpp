#include "block_allocator.h"

#include <kernel/trace.h>
#include <kernel/panic.h>

namespace Kernel
{

BlockAllocatorImpl::BlockAllocatorImpl()
    : Usage(0)
    , Total(0)
    , StartAddress(0)
    , EndAddress(0)
    , BlockSize(0)
{
    BlockList.Init();
}

bool BlockAllocatorImpl::Setup(ulong startAddress, ulong endAddress, ulong blockSize)
{
    Trace(0, "0x%p start 0x%p end 0x%p bsize %u", this, startAddress, endAddress, blockSize);

    Shared::AutoLock lock(Lock);
    if (Total != 0)
        return false;

    if (endAddress <= startAddress)
        return false;

    if ((startAddress % blockSize) != 0)
        return false;

    BlockSize = blockSize;
    StartAddress = EndAddress = startAddress;
    for (ulong blockAddress = startAddress; (blockAddress + BlockSize) <= endAddress; blockAddress+= BlockSize)
    {
        BlockList.InsertTail(reinterpret_cast<ListEntry*>(blockAddress));
        EndAddress = blockAddress + BlockSize;
        Total++;
    }

    Trace(0, "0x%p start 0x%p end 0x%p bsize %u total %u", this, StartAddress, EndAddress, BlockSize, Total);

    return (Total != 0) ? true : false;
}

BlockAllocatorImpl::~BlockAllocatorImpl()
{
    if (Usage != 0)
        Trace(0, "0x%p usage %u blockSize %u", this, Usage, BlockSize);

    BugOn(Usage != 0);
}

void* BlockAllocatorImpl::Alloc()
{
    Shared::AutoLock lock(Lock);

    if (BlockList.IsEmpty())
    {
        return nullptr;
    }

    Usage++;
    return BlockList.RemoveHead();
}

bool BlockAllocatorImpl::IsOwner(void *block)
{
    ulong blockAddr = (ulong)block;
    if (blockAddr < StartAddress)
        return false;

    if (blockAddr >= EndAddress)
        return false;

    if (blockAddr > (EndAddress - BlockSize))
        return false;

    if ((blockAddr % BlockSize) != 0)
        return false;

    return true;
}

void BlockAllocatorImpl::Free(void *block)
{
    BugOn(block == nullptr);
    BugOn(!IsOwner(block));

    Shared::AutoLock lock(Lock);
    BlockList.InsertTail(reinterpret_cast<ListEntry*>(block));
    Usage--;
}

}
