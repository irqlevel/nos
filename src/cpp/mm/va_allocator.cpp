#include "va_allocator.h"
#include "page_table.h"

#include <include/const.h>
#include <kernel/panic.h>
#include <kernel/trace.h>
#include <lib/bitmap.h>

namespace Kernel
{

namespace Mm
{

VaAllocator::VaAllocator()
    : BitmapPtr(nullptr)
    , BitmapSize(0)
    , VaStart(0)
    , VaEnd(0)
    , BlockSize(0)
    , BlockCount(0)
{
}

VaAllocator::~VaAllocator()
{
    Trace(0, "0x%p dtor", this);
}

bool VaAllocator::Setup(ulong vaStart, ulong vaEnd, ulong blockSize)
{
    BlockCount = (vaEnd - vaStart) / blockSize;
    if (!BlockCount)
        return false;

    VaStart = vaStart;
    VaEnd = vaEnd;
    BlockSize = blockSize;
    BitmapSize = (BlockCount + 7) / 8;

    ulong bitmapPages = Stdlib::RoundUp(BitmapSize, Const::PageSize) / Const::PageSize;

    auto& pt = PageTable::GetInstance();
    for (ulong i = 0; i < bitmapPages; i++)
    {
        Page* page = pt.AllocPage();
        if (!page)
        {
            for (ulong j = 0; j < i; j++)
            {
                Page* p = pt.UnmapPage(vaStart + j * Const::PageSize);
                pt.FreePage(p);
                p->Put();
            }
            return false;
        }
        if (!pt.MapPage(vaStart + i * Const::PageSize, page))
        {
            pt.FreePage(page);
            for (ulong j = 0; j < i; j++)
            {
                Page* p = pt.UnmapPage(vaStart + j * Const::PageSize);
                pt.FreePage(p);
                p->Put();
            }
            return false;
        }
    }

    BitmapPtr = (u8*)vaStart;
    Stdlib::MemSet(BitmapPtr, 0, bitmapPages * Const::PageSize);

    /* Reserve the blocks occupied by the bitmap itself */
    ulong bitmapBlocks = Stdlib::RoundUp(bitmapPages * Const::PageSize, blockSize) / blockSize;
    for (ulong i = 0; i < bitmapBlocks; i++)
        Stdlib::Bitmap(BitmapPtr, BlockCount).SetBit(i);

    Trace(0, "0x%p start 0x%p end 0x%p bsize 0x%p bcount %u bmpages %u",
        this, VaStart, VaEnd, BlockSize, BlockCount, bitmapPages);
    return true;
}

ulong VaAllocator::Alloc()
{
    Stdlib::AutoLock lock(Lock);

    long blockIndex = Stdlib::Bitmap(BitmapPtr, BlockCount).FindSetZeroBit();
    if (blockIndex < 0)
        return 0;

    return VaStart + blockIndex * BlockSize;
}

void VaAllocator::Free(ulong va)
{
    Stdlib::AutoLock lock(Lock);

    BugOn(va < VaStart || va >= VaEnd);
    BugOn((va - VaStart) % BlockSize != 0);

    ulong blockIndex = (va - VaStart) / BlockSize;
    Stdlib::Bitmap(BitmapPtr, BlockCount).ClearBit(blockIndex);
}

bool VaAllocator::Contains(ulong va)
{
    return va >= VaStart && va < VaEnd && ((va - VaStart) % BlockSize) == 0;
}

}
}
