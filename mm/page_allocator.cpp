#include "page_allocator.h"
#include "page_table.h"

#include <include/const.h>
#include <kernel/panic.h>
#include <kernel/trace.h>
#include <lib/list_entry.h>
#include <lib/bitmap.h>

namespace Kernel
{

namespace Mm
{

FixedPageAllocator::FixedPageAllocator()
    : VaStart(0)
    , VaEnd(0)
    , PageCount(0)
    , BlockCount(0)
{
    Stdlib::MemSet(BlockBitmap, 0, sizeof(BlockBitmap));
}

FixedPageAllocator::~FixedPageAllocator()
{
    Trace(0, "0x%p dtor", this);
}

bool FixedPageAllocator::Setup(ulong vaStart, ulong vaEnd, ulong pageCount)
{
    BlockCount = Stdlib::Min((vaEnd - vaStart) / (pageCount * Const::PageSize), 8*sizeof(BlockBitmap));
    if (!BlockCount)
        return false;

    VaStart = vaStart;
    VaEnd = vaEnd;
    PageCount = pageCount;
    BlockSize = pageCount * Const::PageSize;

    Trace(0, "0x%p start 0x%p end 0x%p pages %u bcount %u", this, VaStart, VaEnd, PageCount, BlockCount);
    return true;
}

void* FixedPageAllocator::Alloc()
{
    Stdlib::AutoLock lock(Lock);

    auto& pt = PageTable::GetInstance();
    Page* pages[PageCount];

    for (size_t i = 0; i < PageCount; i++)
    {
        pages[i] = pt.AllocPage();
        if (pages[i] == 0)
        {
            for (size_t j = 0; j < i; j++)
            {
                pt.FreePage(pages[j]);
            }
            BugOn(1);
            return nullptr;
        }
    }

    long blockIndex = Stdlib::Bitmap(BlockBitmap, BlockCount).FindSetZeroBit();
    if (blockIndex < 0)
    {
        for (size_t i = 0; i < PageCount; i++)
        {
            pt.FreePage(pages[i]);
        }
        BugOn(1);
        return nullptr;
    }

    for (size_t i = 0; i < PageCount; i++)
    {
        if (!pt.MapPage(VaStart + blockIndex * BlockSize + i * Const::PageSize, pages[i]))
        {
            for (size_t j = 0; j < i; j++)
            {
                Page* page = pt.UnmapPage(VaStart + blockIndex * BlockSize + j * Const::PageSize);
                pt.FreePage(page);
                page->Put();
            }

            for (size_t j = i; j < PageCount; j++)
            {
                pt.FreePage(pages[j]);
            }
            BugOn(1);
            return nullptr;
        }
    }

    void* addr = (void *)(VaStart + blockIndex * BlockSize);
    return addr;
}

bool FixedPageAllocator::Free(void* addr)
{
    Stdlib::AutoLock lock(Lock);

    if ((ulong)addr < VaStart || (ulong)addr >= VaEnd)
        return false;

    BugOn(((ulong)addr % BlockSize) != 0);

    ulong blockIndex = ((ulong)addr - VaStart) / BlockSize;
    Stdlib::Bitmap(BlockBitmap, BlockCount).ClearBit(blockIndex);

    auto& pt = PageTable::GetInstance();
    for (size_t i = 0; i < PageCount; i++)
    {
        Page* page = pt.UnmapPage(VaStart + blockIndex * BlockSize + i * Const::PageSize);
        pt.FreePage(page);
        page->Put();
    }

    return true;
}

PageAllocatorImpl::PageAllocatorImpl()
{
}

bool PageAllocatorImpl::Setup()
{
    auto& pt = PageTable::GetInstance();
    ulong freePagesCount = pt.GetFreePagesCount();
    if (freePagesCount == 0)
        return false;

    ulong startAddress = pt.GetVaEnd();
    ulong endAddress = startAddress + ((7 * freePagesCount) / 10) * Const::PageSize;

    Trace(0, "setup 0x%p start 0x%p end 0x%p free pages %u", this, startAddress, endAddress, freePagesCount);

    size_t sizePerBalloc = (endAddress - startAddress) / Stdlib::ArraySize(FixedPgAlloc);
    for (size_t i = 0; i < Stdlib::ArraySize(FixedPgAlloc); i++)
    {
        ulong start = startAddress + i * sizePerBalloc;
        ulong blockSize = (1UL << i) * Const::PageSize;
        if (!FixedPgAlloc[i].Setup(Stdlib::RoundUp(start, blockSize), start + sizePerBalloc, blockSize / Const::PageSize))
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
    if (log >= Stdlib::ArraySize(FixedPgAlloc))
        return nullptr;

    return FixedPgAlloc[log].Alloc();
}

void PageAllocatorImpl::Free(void* addr)
{
    for (size_t i = 0; i < Stdlib::ArraySize(FixedPgAlloc); i++)
    {
        if (FixedPgAlloc[i].Free(addr))
        {
            return;
        }
    }

    Panic("Can't free addr 0x%p", addr);
}

}
}