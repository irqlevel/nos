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

    Trace(0, "0x%p start 0x%lX end 0x%lX pages %lu", this, vaStart, vaEnd, PageCount);
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

bool FixedPageAllocator::Contains(void* addr)
{
    return VaAlloc.Contains((ulong)addr);
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

    Trace(0, "setup 0x%p start 0x%lX end 0x%lX free pages %lu", this, startAddress, endAddress, freePagesCount);

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

    /* The large runs get a window of their own, past the blocks' */
    const ulong largeBlock = PageTable::MaxLargeMapPages * Const::PageSize;
    const ulong largeStart = Stdlib::RoundUp(endAddress, largeBlock);
    if (!LargePgAlloc.Setup(largeStart, largeStart + LargeBlockCount * largeBlock,
            PageTable::MaxLargeMapPages))
    {
        return false;
    }

    ulong keptPages = Stdlib::Min<ulong>(KeptPagesMax, freePagesCount / KeptRamShare);
    for (size_t i = 0; i < Stdlib::ArraySize(KeptBlocks); i++)
    {
        Stdlib::AutoLock lock(KeptBlocks[i].Lock);
        KeptBlocks[i].Limit = keptPages >> i;
    }

    return true;
}

PageAllocatorImpl::~PageAllocatorImpl()
{
    Trace(0, "0x%p dtor", this);
}

void* PageAllocatorImpl::Alloc(size_t numPages, bool zero)
{
    BugOn(numPages == 0);

    size_t log = Stdlib::Log2(numPages);
    if (log >= Stdlib::ArraySize(FixedPgAlloc))
        return nullptr;

    /* Zeroed when asked, as a block of fresh pages always is: whoever had
       it last wrote into it, and a caller of Mm::Alloc may count on what
       the page allocator has always handed out */
    void* kept = TakeKept(log);
    if (kept != nullptr)
    {
        if (zero)
            Stdlib::MemSet(kept, 0, (1UL << log) * Const::PageSize);
        return kept;
    }

    void* block = FixedPgAlloc[log].Alloc();
    /* No page, or no VA of this size: what the caches hold may be both */
    if (block == nullptr && Trim())
        block = FixedPgAlloc[log].Alloc();
    return block;
}

void PageAllocatorImpl::Free(void* addr)
{
    size_t log;
    if (!ClassOf(addr, log))
        Panic("Can't free addr 0x%p", addr);

    if (!Keep(log, addr))
        Release(addr);
}

bool PageAllocatorImpl::ClassOf(void* addr, size_t& log)
{
    for (size_t i = 0; i < Stdlib::ArraySize(FixedPgAlloc); i++)
    {
        if (FixedPgAlloc[i].Contains(addr))
        {
            log = i;
            return true;
        }
    }
    return false;
}

void PageAllocatorImpl::Release(void* addr)
{
    size_t log;
    if (!ClassOf(addr, log) || !FixedPgAlloc[log].Free(addr))
        Panic("Can't free addr 0x%p", addr);
}

bool PageAllocatorImpl::Keep(size_t log, void* ptr)
{
    BugOn(log >= Stdlib::ArraySize(KeptBlocks));
    auto& list = KeptBlocks[log];
    Kept* block = static_cast<Kept*>(ptr);

    Stdlib::AutoLock lock(list.Lock);
    /* A block on the list carries the mark; one that is not may too, by
       chance, so the mark only says where to look */
    if (block->Mark == KeptMark)
    {
        for (Kept* kept = list.Head; kept != nullptr; kept = kept->Next)
            if (kept == block)
                Panic("Double free of 0x%p", ptr);
    }
    if (list.Count >= list.Limit)
        return false;

    block->Next = list.Head;
    block->Mark = KeptMark;
    list.Head = block;
    list.Count++;
    return true;
}

void* PageAllocatorImpl::TakeKept(size_t log)
{
    BugOn(log >= Stdlib::ArraySize(KeptBlocks));
    auto& list = KeptBlocks[log];

    Stdlib::AutoLock lock(list.Lock);
    Kept* block = list.Head;
    if (block == nullptr)
        return nullptr;
    list.Head = block->Next;
    list.Count--;
    return block;
}

/* Each list is taken whole under its lock and released with the lock let
   go of: a release unmaps, and the shootdown waits on every other CPU */
bool PageAllocatorImpl::Trim()
{
    bool trimmed = false;
    for (size_t log = 0; log < Stdlib::ArraySize(KeptBlocks); log++)
    {
        Kept* head;
        {
            Stdlib::AutoLock lock(KeptBlocks[log].Lock);
            head = KeptBlocks[log].Head;
            KeptBlocks[log].Head = nullptr;
            KeptBlocks[log].Count = 0;
        }

        while (head != nullptr)
        {
            Kept* next = head->Next;
            if (!FixedPgAlloc[log].Free(head))
                Panic("Can't free kept block 0x%p", head);
            head = next;
            trimmed = true;
        }
    }
    return trimmed;
}

void* PageAllocatorImpl::AllocMapPages(size_t numPages, ulong* physAddr)
{
    BugOn(numPages == 0);

    size_t log = Stdlib::Log2(numPages);
    if (log >= Stdlib::ArraySize(FixedPgAlloc))
        return nullptr;

    /* Kept blocks hold pages and this size's VA both: once more without
       them before refusing */
    void* result = TryAllocMapPages(log, physAddr);
    if (result == nullptr && Trim())
        result = TryAllocMapPages(log, physAddr);
    return result;
}

void* PageAllocatorImpl::TryAllocMapPages(size_t log, ulong* physAddr)
{
    size_t roundedPages = 1UL << log;
    auto& pt = PageTable::GetInstance();
    Page* pages = pt.AllocContiguousPages(roundedPages);
    if (!pages)
        return nullptr;

    void* result = FixedPgAlloc[log].Map(pages);
    if (!result)
    {
        for (size_t i = 0; i < roundedPages; i++)
            pt.FreePage(&pages[i]);
        return nullptr;
    }
    *physAddr = pages->GetPhyAddress();
    return result;
}

/* Not kept: a DMA buffer's pages go back to the page allocator as they
   always have */
void PageAllocatorImpl::UnmapFreePages(void* ptr)
{
    Release(ptr);
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
        Panic("Can't unmap addr 0x%p numPages %lu", ptr, numPages);
        return;
    }

    if (!FixedPgAlloc[log].Unmap(ptr, numPages))
        Panic("Can't unmap addr 0x%p numPages %lu", ptr, numPages);
}

void* PageAllocatorImpl::MapLargePages(size_t numPages, ulong* physAddrs)
{
    BugOn(numPages == 0);

    if (numPages > PageTable::MaxLargeMapPages)
        return nullptr;

    return LargePgAlloc.MapPhys(physAddrs, numPages);
}

void PageAllocatorImpl::UnmapLargePages(void* ptr, size_t numPages)
{
    BugOn(numPages == 0 || numPages > PageTable::MaxLargeMapPages);

    if (!LargePgAlloc.Unmap(ptr, numPages))
        Panic("Can't unmap large run 0x%p numPages %lu", ptr, numPages);
}

}
}