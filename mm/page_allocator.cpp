#include "page_allocator.h"

#include <include/const.h>
#include <kernel/panic.h>
#include <kernel/trace.h>
#include <lib/list_entry.h>

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
    Trace(0, "Setup start 0x%p end 0x%p bsize %u", startAddress, endAddress, blockSize);

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

    Trace(0, "Setup start 0x%p end 0x%p bsize %u total %u", StartAddress, EndAddress, BlockSize, Total);

    return (Total != 0) ? true : false;
}

BlockAllocatorImpl::~BlockAllocatorImpl()
{
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

PageAllocatorImpl::PageAllocatorImpl()
{
}

bool PageAllocatorImpl::Setup(ulong startAddress, ulong endAddress)
{
    Trace(0, "Setup start 0x%p end 0x%p", startAddress, endAddress);

    BugOn(endAddress <= startAddress);
    size_t sizePerBalloc = (endAddress - startAddress) / Shared::ArraySize(Balloc);
    for (size_t i = 0; i < Shared::ArraySize(Balloc); i++)
    {
        ulong start = startAddress + i * sizePerBalloc;
        ulong blockSize = ((ulong)1 << i) * Shared::PageSize;
        if (!Balloc[i].Setup(Shared::RoundUp(start, blockSize), start + sizePerBalloc, blockSize))
        {
            return false;
        }
    }

    return true;
}

PageAllocatorImpl::~PageAllocatorImpl()
{
    Trace(0, "0x%p dtor", this);
}

void* PageAllocatorImpl::Alloc(size_t numPages)
{
    BugOn(numPages == 0);

    size_t log = Shared::Log2(numPages);
    if (log >= Shared::ArraySize(Balloc))
        return nullptr;

    return Balloc[log].Alloc();
}

void PageAllocatorImpl::Free(void* pages)
{
    for (size_t i = 0; i < Shared::ArraySize(Balloc); i++)
    {
        auto& balloc = Balloc[i];
        if (balloc.IsOwner(pages))
        {
            balloc.Free(pages);
            return;
        }
    }

    Panic("Can't free pages 0x%p", pages);
}

}