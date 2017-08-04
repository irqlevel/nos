#include "page_allocator.h"

#include <include/const.h>
#include <kernel/panic.h>
#include <kernel/trace.h>
#include <lib/list_entry.h>

namespace Kernel
{

namespace Mm
{

PageAllocatorImpl::PageAllocatorImpl()
{
}

bool PageAllocatorImpl::Setup(ulong startAddress, ulong endAddress)
{
    Trace(0, "Setup start 0x%p end 0x%p", startAddress, endAddress);

    BugOn(endAddress <= startAddress);
    size_t sizePerBalloc = (endAddress - startAddress) / Stdlib::ArraySize(Balloc);
    for (size_t i = 0; i < Stdlib::ArraySize(Balloc); i++)
    {
        ulong start = startAddress + i * sizePerBalloc;
        ulong blockSize = ((ulong)1 << i) * Const::PageSize;
        if (!Balloc[i].Setup(Stdlib::RoundUp(start, blockSize), start + sizePerBalloc, blockSize))
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

    size_t log = Stdlib::Log2(numPages);
    if (log >= Stdlib::ArraySize(Balloc))
        return nullptr;

    return Balloc[log].Alloc();
}

void PageAllocatorImpl::Free(void* pages)
{
    for (size_t i = 0; i < Stdlib::ArraySize(Balloc); i++)
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
}