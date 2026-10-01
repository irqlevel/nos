// The kernel heap (mm/allocator.cpp, pool.cpp, page_allocator.cpp,
// va_allocator.cpp): Mm::Alloc and Mm::Free, the pools of every size class
// with their lists threaded through the blocks themselves, the page runs
// behind them, the VA allocators' bitmaps, and the three ways a driver maps
// pages -- everything every allocation of the kernel goes through, and the
// allocation failures nothing else makes happen.
//
// Under them, the page table as page_table.h promises it (a page run
// allocated, mapped, unmapped, freed), over pages that are only names and a
// VA region of the host's own: a VA the heap maps is host memory it may use,
// zeroed as the kernel's are; one it does not is poisoned for ASan, so the
// heap reaching past what it mapped is a report. Pages and maps fail on the
// input's word.
//
// A random sequence of allocations of every size -- the pools', the edges
// between them, whole pages, runs past the largest -- frees, page runs and
// driver mappings is held to what an allocator is: every block it hands out
// the size asked for, aligned, inside what is mapped and nobody else's;
// what is written into a block there when it is freed; nothing refused while
// there is memory to give; and once everything is freed, every page back
// but the bitmaps', whatever failed on the way.
#include "host.h"

/* The heap's singletons have private constructors, which Reset runs again
   in place. */
#define private public
#include <mm/allocator.h>
#include <mm/page_allocator.h>
#include <mm/page_table.h>
#include <mm/va_allocator.h>
#undef private

#include <kernel/cpu.h>

#include "fuzz.h"
#include "kernel.h"

#include <sanitizer/asan_interface.h>
#include <sys/mman.h>

using namespace Kernel;
using Kernel::Mm::AllocatorImpl;
using Kernel::Mm::Page;
using Kernel::Mm::PageAllocatorImpl;
using Kernel::Mm::PageTable;

namespace
{

const ulong PageBytes = Const::PageSize;

/* The host VA the heap's VA allocators are given: reserved once, used a
   few megabytes of per input, and the large runs' window past them. */
const ulong RegionBytes = 2UL << 30;
uint8_t* Region;

/* The pages: names, a Page each, as PageArray has them */
struct PageSet
{
    Page* Array = nullptr;
    ulong Count = 0;

    Page* data() { return Array; }
    ulong size() const { return Count; }
    Page& operator[](ulong i) { return Array[i]; }
};

struct Pages
{
    PageSet All;
    std::vector<bool> Free;
    /* Free pages, most recently freed on top; some may have been taken by
       AllocContiguousPages since, and are passed over */
    std::vector<ulong> Stack;
    /* How many VAs each page is mapped at */
    std::vector<ulong> MapCount;
    ulong FreeCount = 0;
    long FailAfter = -1;
    long MapFailAfter = -1;
    /* va -> page index, for every page mapped */
    std::map<ulong, ulong> Mapped;
} P;

bool Take(long& failAfter)
{
    if (failAfter == 0)
        return false;
    if (failAfter > 0)
        failAfter--;
    return true;
}

ulong IndexOf(Page* page)
{
    ulong i = static_cast<ulong>(page - P.All.data());
    INVARIANT(i < P.All.size(), "a page that is no page: %p", page);
    return i;
}

void MapOne(ulong va, ulong index)
{
    INVARIANT(va % PageBytes == 0, "a map at 0x%lx, not a page's start", va);
    INVARIANT(va >= reinterpret_cast<ulong>(Region) && va + PageBytes <= reinterpret_cast<ulong>(Region) + RegionBytes,
        "a map at 0x%lx, outside the VA the heap was given", va);
    INVARIANT(!P.Free[index], "free page %lu mapped at 0x%lx", index, va);
    INVARIANT(P.Mapped.insert({va, index}).second, "0x%lx mapped twice", va);
    P.MapCount[index]++;
    ASAN_UNPOISON_MEMORY_REGION(reinterpret_cast<void*>(va), PageBytes);
    memset(reinterpret_cast<void*>(va), 0, PageBytes);
}

ulong UnmapOne(ulong va)
{
    auto it = P.Mapped.find(va);
    INVARIANT(it != P.Mapped.end(), "an unmap of 0x%lx, which is not mapped", va);
    ulong index = it->second;
    P.Mapped.erase(it);
    P.MapCount[index]--;
    ASAN_POISON_MEMORY_REGION(reinterpret_cast<void*>(va), PageBytes);
    return index;
}

}

/* ---- the page table, as the heap uses it ---- */

namespace Kernel
{
namespace Mm
{

PageTable::PageTable()
{
}

PageTable::~PageTable()
{
}

Page* PageTable::AllocPage()
{
    INVARIANT(Fuzz::HeldSpinLocks() == 0, "a page allocated with a spin lock held");
    if (!Take(P.FailAfter) || P.FreeCount == 0)
        return nullptr;
    while (!P.Stack.empty())
    {
        ulong i = P.Stack.back();
        P.Stack.pop_back();
        if (P.Free[i])
        {
            P.Free[i] = false;
            P.FreeCount--;
            return &P.All[i];
        }
    }
    INVARIANT(false, "%lu pages free and none on the stack", P.FreeCount);
    return nullptr;
}

Page* PageTable::AllocContiguousPages(ulong count)
{
    if (!Take(P.FailAfter) || count == 0 || count > MaxContiguousPages)
        return nullptr;
    for (ulong i = 0; i + count <= P.All.size(); i++)
    {
        ulong n = 0;
        while (n < count && P.Free[i + n])
            n++;
        if (n == count)
        {
            for (ulong j = 0; j < count; j++)
                P.Free[i + j] = false;
            P.FreeCount -= count;
            return &P.All[i];
        }
        i += n;
    }
    return nullptr;
}

void PageTable::FreePage(Page* page)
{
    ulong i = IndexOf(page);
    INVARIANT(!P.Free[i], "page %lu freed twice", i);
    INVARIANT(P.MapCount[i] == 0, "page %lu freed while mapped", i);
    P.Free[i] = true;
    P.FreeCount++;
    P.Stack.push_back(i);
}

ulong PageTable::GetFreePagesCount()
{
    return P.FreeCount;
}

ulong PageTable::GetVaEnd()
{
    return reinterpret_cast<ulong>(Region);
}

bool PageTable::MapPage(ulong virtAddr, Page* page)
{
    return MapPages(virtAddr, &page, 1);
}

bool PageTable::MapPages(ulong virtAddr, Page* const* pages, size_t count)
{
    INVARIANT(Fuzz::HeldSpinLocks() == 0, "pages mapped with a spin lock held");
    if (!Take(P.MapFailAfter))
        return false;
    for (size_t i = 0; i < count; i++)
        MapOne(virtAddr + i * PageBytes, IndexOf(pages[i]));
    return true;
}

bool PageTable::MapContiguousPages(ulong virtAddr, Page* pages, size_t count)
{
    if (!Take(P.MapFailAfter))
        return false;
    for (size_t i = 0; i < count; i++)
        MapOne(virtAddr + i * PageBytes, IndexOf(&pages[i]));
    return true;
}

bool PageTable::MapPhysPages(ulong virtAddr, const ulong* phyAddrs, size_t count)
{
    if (!Take(P.MapFailAfter))
        return false;
    for (size_t i = 0; i < count; i++)
    {
        INVARIANT(phyAddrs[i] % PageBytes == 0 && phyAddrs[i] / PageBytes < P.All.size(),
            "a map of physical 0x%lx, which is no page", phyAddrs[i]);
        MapOne(virtAddr + i * PageBytes, phyAddrs[i] / PageBytes);
    }
    return true;
}

void PageTable::UnmapPages(ulong virtAddr, size_t count, bool freePages)
{
    for (size_t i = 0; i < count; i++)
    {
        ulong index = UnmapOne(virtAddr + i * PageBytes);
        if (freePages)
            FreePage(&P.All[index]);
    }
}

Page* PageTable::UnmapPage(ulong virtAddr)
{
    return &P.All[UnmapOne(virtAddr)];
}

}

void CpuTable::InvalidateTlbRange(ulong virtAddr, ulong count)
{
    (void)virtAddr;
    (void)count;
}

}

namespace
{

/* ---- the model ---- */

struct Block
{
    ulong Size;
    uint8_t Mark;
    enum Kind
    {
        Heap,
        Run,    /* AllocMapPages */
        Driver, /* MapPages of the sequence's own pages */
        Large,  /* MapLargePages */
    } How;
    std::vector<ulong> Pages;
};

std::map<ulong, Block> Live;
ulong Baseline;

AllocatorImpl& Heap()
{
    return AllocatorImpl::GetInstance(&PageAllocatorImpl::GetInstance());
}

bool IsMapped(ulong va, ulong size)
{
    for (ulong p = va & ~(PageBytes - 1); p < va + size; p += PageBytes)
    {
        if (!P.Mapped.count(p))
            return false;
    }
    return true;
}

void Fill(ulong va, const Block& b)
{
    memset(reinterpret_cast<void*>(va), b.Mark, b.Size);
}

void CheckFill(ulong va, const Block& b, const char* when)
{
    const uint8_t* p = reinterpret_cast<const uint8_t*>(va);
    for (ulong i = 0; i < b.Size; i++)
        INVARIANT(p[i] == b.Mark, "the block at 0x%lx of %lu bytes changed at +%lu %s", va, b.Size, i, when);
}

void Place(ulong va, Block b, const char* by)
{
    INVARIANT(IsMapped(va, b.Size), "%s gave 0x%lx for %lu bytes, which is not all mapped", by, va, b.Size);
    auto next = Live.lower_bound(va);
    if (next != Live.end())
        INVARIANT(va + b.Size <= next->first, "%s gave [0x%lx, +%lu), which runs into the block at 0x%lx", by, va,
            b.Size, next->first);
    if (next != Live.begin())
    {
        auto prev = std::prev(next);
        INVARIANT(prev->first + prev->second.Size <= va, "%s gave 0x%lx, inside the block at 0x%lx", by, va,
            prev->first);
    }
    Fill(va, b);
    Live[va] = b;
}

ulong Size(Fuzz::Input& in)
{
    switch (in.U8() % 10)
    {
    case 0:
        return 1 + in.Below(16);
    case 1:
    case 2:
        return 1 + in.Below(256);
    case 3:
        return 1 + in.Below(2048);
    case 4:
        /* The edge between the pools and whole pages */
        return 2032 + in.Below(32);
    case 5:
        return PageBytes * (1 + in.Below(4)) - 8 + in.Below(16);
    case 6:
        return 1 + in.Below(PageBytes * PageTable::MaxContiguousPages + PageBytes);
    default:
        return 1 + in.Below(512);
    }
}

/* Blocks of a fixed page allocator's VA nobody holds: its bitmap's zeros */
ulong VaFree(ulong log)
{
    auto& va = PageAllocatorImpl::GetInstance().FixedPgAlloc[log].VaAlloc;
    ulong free = 0;
    for (ulong i = 0; i < va.BlockCount; i++)
        free += ((va.BitmapPtr[i / 8] >> (i % 8)) & 1) == 0;
    return free;
}

/* Blocks every CPU's cache holds */
ulong CachedCount()
{
    ulong n = 0;
    for (auto& cache : Heap().Caches)
        for (auto& size : cache.Sizes)
            n += size.Count;
    return n;
}

/* Blocks the page allocator keeps, of every size */
ulong KeptCount()
{
    auto& pa = PageAllocatorImpl::GetInstance();
    ulong n = 0;
    for (auto& list : pa.KeptBlocks)
        n += list.Count;
    for (auto& cpu : pa.CpuKeptBlocks)
        for (auto& size : cpu.Sizes)
            n += size.Count;
    return n;
}

/* Blocks every CPU's cache of kept blocks holds */
ulong CpuKeptCount()
{
    ulong n = 0;
    for (auto& cpu : PageAllocatorImpl::GetInstance().CpuKeptBlocks)
        for (auto& size : cpu.Sizes)
            n += size.Count;
    return n;
}

/* A kept block as it must be, wherever it is kept: of its size's VA, all
   mapped, marked, and nobody's in use; and kept once */
void CheckKeptBlock(PageAllocatorImpl& pa, ulong log, ulong va, std::set<ulong>& seen)
{
    auto* kept = reinterpret_cast<PageAllocatorImpl::Kept*>(va);
    INVARIANT(pa.FixedPgAlloc[log].Contains(kept), "a kept block 0x%lx not of its size's VA", va);
    INVARIANT(IsMapped(va, (1UL << log) * PageBytes), "a kept block 0x%lx not all mapped", va);
    INVARIANT(kept->Mark == PageAllocatorImpl::KeptMark, "a kept block 0x%lx without its mark", va);
    auto live = Live.upper_bound(va + (1UL << log) * PageBytes - 1);
    INVARIANT(live == Live.begin() || std::prev(live)->first + std::prev(live)->second.Size <= va,
        "a kept block 0x%lx that a block in use overlaps", va);
    INVARIANT(seen.insert(va).second, "the block 0x%lx kept twice", va);
}

/* What the heap keeps for its next allocations, between two operations:
   no more than its bounds, each kept block whole -- mapped, marked, its own
   and nobody's in use -- and each pool's kept pages empty */
void CheckKept()
{
    auto& pa = PageAllocatorImpl::GetInstance();
    std::set<ulong> kept;
    for (ulong log = 0; log < Stdlib::ArraySize(pa.KeptBlocks); log++)
    {
        auto& list = pa.KeptBlocks[log];
        INVARIANT(list.Count <= list.Limit, "%lu blocks of %lu pages kept, past the bound of %lu", list.Count,
            1UL << log, list.Limit);
        ulong n = 0;
        for (auto* block = list.Head; block != nullptr; block = block->Next)
        {
            INVARIANT(++n <= list.Count, "the list of kept %lu-page blocks is longer than its count %lu",
                1UL << log, list.Count);
            CheckKeptBlock(pa, log, reinterpret_cast<ulong>(block), kept);
        }
        INVARIANT(n == list.Count, "%lu kept %lu-page blocks on a list that counts %lu", n, 1UL << log,
            list.Count);
    }

    /* Each CPU's: within its bound, which is within KeptBlocks' */
    for (ulong cpu = 0; cpu < Stdlib::ArraySize(pa.CpuKeptBlocks); cpu++)
    {
        for (ulong log = 0; log < PageAllocatorImpl::CpuKeptLogs; log++)
        {
            auto& size = pa.CpuKeptBlocks[cpu].Sizes[log];
            INVARIANT(size.Count <= size.Limit && size.Limit <= pa.KeptBlocks[log].Limit,
                "CPU %lu keeps %lu blocks of %lu pages, its bound %lu and the shared one %lu", cpu, size.Count,
                1UL << log, size.Limit, pa.KeptBlocks[log].Limit);
            for (ulong i = 0; i < size.Count; i++)
                CheckKeptBlock(pa, log, reinterpret_cast<ulong>(size.Blocks[i]), kept);
        }
    }

    auto& heap = Heap();
    for (auto& pool : heap.Pool)
        INVARIANT(pool.EmptyPages <= Kernel::Mm::Pool::EmptyPagesKept, "a pool of %lu-byte blocks keeps %lu empty "
            "pages", pool.BlockSize, pool.EmptyPages);

    /* Every CPU's cache: within its bound, each block in it free -- its tag
       the pool's FreedTag -- in no other cache and nobody's in use (whose
       address is the block's past the heap's header) */
    std::set<ulong> cached;
    for (ulong cpu = 0; cpu < Stdlib::ArraySize(heap.Caches); cpu++)
    {
        for (ulong p = 0; p < AllocatorImpl::PoolCount; p++)
        {
            auto& size = heap.Caches[cpu].Sizes[p];
            INVARIANT(size.Count <= size.Limit, "CPU %lu caches %lu blocks of pool %lu, past its bound of %lu", cpu,
                size.Count, p, size.Limit);
            for (ulong i = 0; i < size.Count; i++)
            {
                ulong block = reinterpret_cast<ulong>(size.Blocks[i]);
                auto* header = reinterpret_cast<Kernel::Mm::Pool::Block*>(block) - 1;
                INVARIANT(static_cast<ulong>(header->Tag.Get()) == Kernel::Mm::Pool::FreedTag,
                    "CPU %lu caches 0x%lx, whose tag 0x%lx is not a free block's", cpu, block,
                    static_cast<ulong>(header->Tag.Get()));
                INVARIANT(cached.insert(block).second, "0x%lx cached twice", block);
                INVARIANT(!Live.count(block + sizeof(AllocatorImpl::Header)), "CPU %lu caches 0x%lx, which is in use",
                    cpu, block);
            }
        }
    }
}

void Alloc(Fuzz::Input& in)
{
    ulong size = Size(in);
    /* Rust's allocations, which ask for no contents in particular */
    const bool uninit = in.Bool();
    ulong keptBefore = KeptCount();
    ulong cachedBefore = CachedCount();
    ulong cpuKeptBefore = CpuKeptCount();
    void* p = uninit ? Heap().AllocUninit(size, 0x54657374) : Heap().Alloc(size, 0x54657374);
    if (CpuKeptCount() < cpuKeptBefore)
        Fuzz::Reached("a kept block from a CPU's own");
    if (KeptCount() < keptBefore)
        Fuzz::Reached(uninit ? "a kept block handed out as it was" : "a kept block handed out zeroed");
    if (CachedCount() < cachedBefore)
        Fuzz::Reached("a block from a CPU's cache");
    if (p == nullptr)
    {
        /* Past the largest run, no page or no VA for one -- a pool's block
           may need a page of its own -- or a failure on the input's word */
        ulong pages = (size + 8 >= PageBytes / 2) ? (size + PageBytes - 1) / PageBytes : 1;
        ulong log = Stdlib::Log2(pages);
        INVARIANT(pages > PageTable::MaxContiguousPages || P.FreeCount < (1UL << log) || VaFree(log) == 0 ||
            P.FailAfter >= 0 || P.MapFailAfter >= 0, "Alloc(%lu) refused with %lu pages and %lu blocks of VA free",
            size, P.FreeCount, VaFree(log));
        Fuzz::Reached("an allocation refused");
        return;
    }
    ulong va = reinterpret_cast<ulong>(p);
    INVARIANT(va % 8 == 0, "Alloc(%lu) gave 0x%lx, not 8-aligned", size, va);
    const bool pages = size + 8 >= PageBytes / 2;
    if (pages && !uninit)
    {
        /* Zeroed, as fresh pages are -- a block the page allocator kept
           from a free included, whatever was written into it before */
        INVARIANT(IsMapped(va, size), "Alloc gave 0x%lx for %lu bytes, which is not all mapped", va, size);
        const uint8_t* bytes = reinterpret_cast<const uint8_t*>(va);
        for (ulong i = 0; i < size; i++)
            INVARIANT(bytes[i] == 0, "Alloc(%lu) gave 0x%lx with a byte not zero at +%lu", size, va, i);
    }
    Place(va, {size, static_cast<uint8_t>(1 + in.Below(255)), Block::Heap, {}}, "Alloc");
    Fuzz::Reached(pages ? "a page allocation" : "a pool allocation");
}

void Free(Fuzz::Input& in)
{
    if (Live.empty())
        return;
    auto it = Live.begin();
    std::advance(it, in.Below(Live.size()));
    ulong va = it->first;
    Block b = it->second;
    CheckFill(va, b, "before its free");
    Live.erase(it);
    auto& pa = PageAllocatorImpl::GetInstance();
    switch (b.How)
    {
    case Block::Heap:
    {
        ulong cachedBefore = CachedCount();
        Heap().Free(reinterpret_cast<void*>(va));
        if (CachedCount() < cachedBefore)
            Fuzz::Reached("a CPU's cache spilled to its pool");
        break;
    }
    case Block::Run:
        pa.UnmapFreePages(reinterpret_cast<void*>(va));
        break;
    case Block::Driver:
        pa.UnmapPages(reinterpret_cast<void*>(va), b.Pages.size());
        for (ulong i : b.Pages)
            PageTable::GetInstance().FreePage(&P.All[i]);
        break;
    case Block::Large:
        pa.UnmapLargePages(reinterpret_cast<void*>(va), b.Pages.size());
        for (ulong i : b.Pages)
            PageTable::GetInstance().FreePage(&P.All[i]);
        break;
    }
}

void PageRun(Fuzz::Input& in)
{
    ulong count = 1 + in.Below(in.Chance(32) ? 200 : 12);
    ulong phys = 0;
    void* p = PageAllocatorImpl::GetInstance().AllocMapPages(count, &phys);
    if (p == nullptr)
    {
        Fuzz::Reached("a page run refused");
        return;
    }
    ulong va = reinterpret_cast<ulong>(p);
    INVARIANT(va % PageBytes == 0, "AllocMapPages gave 0x%lx", va);
    /* The run is contiguous, from phys on, and all of it mapped in order */
    ulong rounded = 1;
    while (rounded < count)
        rounded <<= 1;
    std::vector<ulong> pages;
    for (ulong i = 0; i < rounded; i++)
    {
        auto m = P.Mapped.find(va + i * PageBytes);
        INVARIANT(m != P.Mapped.end() && m->second == phys / PageBytes + i,
            "page %lu of the run at 0x%lx is not physical 0x%lx", i, va, phys + i * PageBytes);
        pages.push_back(m->second);
    }
    Place(va, {rounded * PageBytes, static_cast<uint8_t>(1 + in.Below(255)), Block::Run, pages}, "AllocMapPages");
    Fuzz::Reached("a page run");
}

void Driver(Fuzz::Input& in)
{
    const bool large = in.Chance(32);
    ulong count = large ? 1 + in.Below(300) : 1 + in.Below(8);
    std::vector<ulong> pages;
    std::vector<ulong> phys;
    for (ulong i = 0; i < count; i++)
    {
        Page* page = PageTable::GetInstance().AllocPage();
        if (page == nullptr)
            break;
        pages.push_back(IndexOf(page));
        phys.push_back(IndexOf(page) * PageBytes);
    }
    void* p = nullptr;
    if (!pages.empty())
    {
        auto& pa = PageAllocatorImpl::GetInstance();
        p = large ? pa.MapLargePages(pages.size(), phys.data()) : pa.MapPages(pages.size(), phys.data());
    }
    if (p == nullptr)
    {
        for (ulong i : pages)
            PageTable::GetInstance().FreePage(&P.All[i]);
        Fuzz::Reached("a driver's map refused");
        return;
    }
    ulong va = reinterpret_cast<ulong>(p);
    for (ulong i = 0; i < pages.size(); i++)
    {
        auto m = P.Mapped.find(va + i * PageBytes);
        INVARIANT(m != P.Mapped.end() && m->second == pages[i], "a driver's page %lu mapped elsewhere", i);
    }
    Place(va, {pages.size() * PageBytes, static_cast<uint8_t>(1 + in.Below(255)), large ? Block::Large : Block::Driver,
               pages}, large ? "MapLargePages" : "MapPages");
    Fuzz::Reached(large ? "a large driver's map" : "a driver's map");
}

void Boot(Fuzz::Input& in)
{
    /* 32 MiB and up: a smaller machine gives the largest runs' allocator no
       VA to set up over, and the kernel would not boot on it */
    ulong pages = 8192 + in.Below(8192);
    P.All.Array = new Page[pages];
    P.All.Count = pages;
    for (ulong i = 0; i < pages; i++)
        P.All[i].Init(i * PageBytes);
    P.Free.assign(pages, true);
    P.MapCount.assign(pages, 0);
    P.FreeCount = pages;
    for (ulong i = pages; i-- > 0;)
        P.Stack.push_back(i);
    /* The fixed allocators' ranges and the first of the large runs' window:
       poisoned until mapped */
    ASAN_POISON_MEMORY_REGION(Region, 64UL << 20);
    INVARIANT(PageAllocatorImpl::GetInstance().Setup(), "the page allocator would not set up over %lu pages", pages);
    Heap();
    Baseline = pages - P.FreeCount;
}

void Run(Fuzz::Input& in)
{
    Boot(in);
    for (int ops = 0; ops < 200 && in.More(); ops++)
    {
        /* Which CPU the operation runs on: a block freed on another than
           the one it was taken on moves between their caches; past the
           caches' CPUs there is none, and the pools are reached directly */
        if (in.Chance(32))
            Fuzz::CurrentCpu = in.Chance(16) ? Kernel::Mm::CacheCpus + in.Below(4) : in.Below(4);
        switch (in.U8() % 13)
        {
        case 12:
            /* What the heap keeps for its next allocations given back,
               with blocks still out */
            if (Heap().Trim())
                Fuzz::Reached("a trim that gave something back");
            break;
        case 0:
        case 1:
        case 2:
        case 3:
        case 4:
            Alloc(in);
            break;
        case 5:
        case 6:
        case 7:
            Free(in);
            break;
        case 8:
            PageRun(in);
            break;
        case 9:
            Driver(in);
            break;
        case 10:
            /* Pages and maps that fail, from a few calls on */
            if (!in.Chance(48))
                break;
            if (in.Bool())
                P.FailAfter = in.Below(4);
            else
                P.MapFailAfter = in.Below(3);
            Fuzz::Reached("failures armed");
            break;
        default:
            P.FailAfter = -1;
            P.MapFailAfter = -1;
            break;
        }
        Fuzz::CheckNoLocksHeld("an operation");
        CheckKept();
    }

    /* Everything freed and the heap trimmed: every block as it was written,
       and every page back but the VA allocators' bitmaps -- none left in
       a pool's kept page or among the page allocator's kept blocks */
    P.FailAfter = -1;
    P.MapFailAfter = -1;
    for (auto& l : Live)
        CheckFill(l.first, l.second, "at the end");
    while (!Live.empty())
        Free(in);
    Heap().Trim();
    INVARIANT(P.All.size() - P.FreeCount == Baseline, "%lu pages still out once everything is freed, where the "
        "bitmaps hold %lu", P.All.size() - P.FreeCount, Baseline);
    INVARIANT(!Heap().Trim(), "a second trim found something the first left");
}

void Reset()
{
    for (auto& m : P.Mapped)
        ASAN_POISON_MEMORY_REGION(reinterpret_cast<void*>(m.first), PageBytes);
    delete[] P.All.Array;
    P = Pages();
    Live.clear();
    auto& heap = AllocatorImpl::GetInstance(&PageAllocatorImpl::GetInstance());
    heap.~AllocatorImpl();
    new (&heap) AllocatorImpl(&PageAllocatorImpl::GetInstance());
    auto& pa = PageAllocatorImpl::GetInstance();
    pa.~PageAllocatorImpl();
    new (&pa) PageAllocatorImpl();
}

/* The region, once: host memory the heap's VAs are in */
struct RegionInit
{
    RegionInit()
    {
        void* m = mmap(nullptr, RegionBytes + (16UL << 20), PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON, -1, 0);
        INVARIANT(m != MAP_FAILED, "no host VA for the heap");
        Region = reinterpret_cast<uint8_t*>((reinterpret_cast<ulong>(m) + (16UL << 20) - 1) & ~((16UL << 20) - 1));
    }
} TheRegion;

}

const Fuzz::Target Fuzz::TheTarget = {"heap", Reset, Run, 2048, 5000};
