#include "page_allocator.h"
#include "page_table.h"

#include <include/const.h>
#include <kernel/panic.h>
#include <kernel/trace.h>
#include <kernel/cpu.h>
#include <lib/list_entry.h>

namespace Kernel
{

namespace Mm
{

FixedPageAllocator::FixedPageAllocator()
    : PageCount(0)
{
}

FixedPageAllocator::~FixedPageAllocator()
{
    Trace(0, "0x%p dtor", this);
}

bool FixedPageAllocator::Setup(ulong vaStart, ulong vaEnd, ulong pageCount)
{
    PageCount = pageCount;
    ulong blockSize = pageCount * Const::PageSize;

    Trace(0, "0x%p start 0x%p end 0x%p pages %u", this, vaStart, vaEnd, PageCount);
    return VaAlloc.Setup(vaStart, vaEnd, blockSize);
}

void* FixedPageAllocator::Alloc()
{
    auto& pt = PageTable::GetInstance();
    BugOn(PageCount > MaxPageCount);
    Page* pages[MaxPageCount];

    for (size_t i = 0; i < PageCount; i++)
    {
        pages[i] = pt.AllocPage();
        if (pages[i] == 0)
        {
            for (size_t j = 0; j < i; j++)
                pt.FreePage(pages[j]);
            return nullptr;
        }
    }

    ulong va = VaAlloc.Alloc();
    if (va == 0)
    {
        for (size_t i = 0; i < PageCount; i++)
            pt.FreePage(pages[i]);
        return nullptr;
    }

    if (!pt.MapPages(va, pages, PageCount))
    {
        for (size_t i = 0; i < PageCount; i++)
            pt.FreePage(pages[i]);
        /* Remote CPUs may have cached translations from the prefix MapPages
           rolled back (it only invalidates locally); flush everywhere before
           the VA block and the pages become reusable. */
        Kernel::CpuTable::GetInstance().InvalidateTlbRange(va, PageCount);
        VaAlloc.Free(va);
        return nullptr;
    }

    return (void*)va;
}

void* FixedPageAllocator::Map(Page* pages)
{
    ulong va = VaAlloc.Alloc();
    if (va == 0)
    {
        return nullptr;
    }

    auto& pt = PageTable::GetInstance();
    if (!pt.MapContiguousPages(va, pages, PageCount))
    {
        /* See Alloc: shoot down remote TLBs before reusing the VA. */
        Kernel::CpuTable::GetInstance().InvalidateTlbRange(va, PageCount);
        VaAlloc.Free(va);
        return nullptr;
    }

    return (void*)va;
}

void* FixedPageAllocator::MapPhys(ulong* physAddrs, size_t count)
{
    BugOn(count == 0 || count > PageCount);

    ulong va = VaAlloc.Alloc();
    if (va == 0)
    {
        return nullptr;
    }

    auto& pt = PageTable::GetInstance();
    if (!pt.MapPhysPages(va, physAddrs, count))
    {
        /* See Alloc: shoot down remote TLBs before reusing the VA. */
        Kernel::CpuTable::GetInstance().InvalidateTlbRange(va, count);
        VaAlloc.Free(va);
        return nullptr;
    }

    return (void*)va;
}

bool FixedPageAllocator::Unmap(void* addr, size_t count)
{
    if (!VaAlloc.Contains((ulong)addr))
        return false;

    BugOn(count == 0 || count > PageCount);

    PageTable::GetInstance().UnmapPages((ulong)addr, count, false);

    Kernel::CpuTable::GetInstance().InvalidateTlbRange((ulong)addr, count);
    VaAlloc.Free((ulong)addr);
    return true;
}

bool FixedPageAllocator::Free(void* addr)
{
    if (!VaAlloc.Contains((ulong)addr))
        return false;

    PageTable::GetInstance().UnmapPages((ulong)addr, PageCount, true);

    Kernel::CpuTable::GetInstance().InvalidateTlbRange((ulong)addr, PageCount);
    VaAlloc.Free((ulong)addr);
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

void* PageAllocatorImpl::AllocMapPages(size_t numPages, ulong* physAddr)
{
    BugOn(numPages == 0);

    size_t log = Stdlib::Log2(numPages);
    if (log >= Stdlib::ArraySize(FixedPgAlloc))
        return nullptr;

    size_t roundedPages = 1UL << log;
    auto& pt = PageTable::GetInstance();
    Page* pages = pt.AllocContiguousPages(roundedPages);
    if (!pages)
        return nullptr;

    *physAddr = pages->GetPhyAddress();
    void* result = FixedPgAlloc[log].Map(pages);
    if (!result)
    {
        for (size_t i = 0; i < roundedPages; i++)
            pt.FreePage(&pages[i]);
        return nullptr;
    }
    return result;
}

void PageAllocatorImpl::UnmapFreePages(void* ptr)
{
    Free(ptr);
}

void* PageAllocatorImpl::MapPages(size_t numPages, ulong* physAddrs)
{
    BugOn(numPages == 0);

    size_t log = Stdlib::Log2(numPages);
    if (log >= Stdlib::ArraySize(FixedPgAlloc))
        return nullptr;

    return FixedPgAlloc[log].MapPhys(physAddrs, numPages);
}

void PageAllocatorImpl::UnmapPages(void* ptr, size_t numPages)
{
    BugOn(numPages == 0);

    size_t log = Stdlib::Log2(numPages);
    if (log >= Stdlib::ArraySize(FixedPgAlloc))
    {
        Panic("Can't unmap addr 0x%p numPages %u", ptr, numPages);
        return;
    }

    if (!FixedPgAlloc[log].Unmap(ptr, numPages))
        Panic("Can't unmap addr 0x%p numPages %u", ptr, numPages);
}

}
}