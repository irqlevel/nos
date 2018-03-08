#include "page_allocator.h"
#include "page_table.h"

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

bool PageAllocatorImpl::Setup()
{
    auto& pt = PageTable::GetInstance();
    ulong availablePages = pt.GetAvailableFreePages();
    if (availablePages == 0)
        return false;

    ulong startAddress = pt.GetTmpMapEnd();
    ulong endAddress = startAddress + ((7 * availablePages) / 10) * Const::PageSize;

    Trace(0, "Setup 0x%p start 0x%p end 0x%p pages %u", this, startAddress, endAddress, availablePages);

    for (ulong va = startAddress; va < endAddress; va += Const::PageSize)
    {
        ulong page = pt.AllocPage();
        if (!page)
            return false;

        if (!pt.MapPage(va, page))
            return false;
    }

    size_t sizePerBalloc = (endAddress - startAddress) / Stdlib::ArraySize(Balloc);
    for (size_t i = 0; i < Stdlib::ArraySize(Balloc); i++)
    {
        ulong start = startAddress + i * sizePerBalloc;
        ulong blockSize = (1UL << i) * Const::PageSize;
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