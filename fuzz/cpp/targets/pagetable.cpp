// The page tables and the physical page allocator (mm/page_table.cpp), past
// Setup: what every mapping, every page and every frame of the running
// kernel goes through, and the error paths nothing else takes -- a table
// that cannot be allocated half way down a walk, a TmpMap window with no slot
// left, a machine with no free page.
//
// The machine is a small one: physical memory is a host buffer, PageArray a
// host array, and the state PageTable::Setup leaves -- the root, the free
// list, the TmpMap window and the L1 table that maps it -- is set up here as
// Setup documents it, since Setup itself runs on the bootstrap linear map. The
// MMU is emulated where the code reaches memory through it, the TmpMap
// window: when the kernel changes a slot's entry and tells the hardware (a
// TLB flush, an entry made valid), the slot is loaded with the page it now
// maps, and the one it mapped before gets the slot's contents back; an unmapped
// slot is poisoned for ASan, so a read through a stale window is a report.
//
// A random sequence of what the kernel does -- pages allocated and freed, one
// at a time and in contiguous runs, mapped and unmapped in every form, their
// protection changed, looked up, temp-mapped and written through, frames
// handed out, copied through a CPU's frame slot and freed, MMIO mapped -- is
// held to a model: every page's reference count as mm/page_table.h's rules
// have it, every mapping's translation, every page either free, the
// sequence's, a table, or the machine's own, and never two; a page handed out
// zeroed; a map that fails leaving nothing mapped; no TmpMap slot left behind
// by any path.
#include "host.h"

/* The page table and the memory map are singletons with private
   constructors, which Reset runs again in place; and the page table's
   state after Setup is set up here, member by member. */
#define private public
#include <mm/memory_map.h>
#include <mm/page_table.h>
#undef private

#include <hal/cpu.h>
#include <hal/irqchip.h>
#include <hal/pte.h>
#include <kernel/preempt.h>
#include <lib/printer.h>

#include "fuzz.h"
#include "kernel.h"

#include <sanitizer/asan_interface.h>

using namespace Kernel;
using Kernel::Mm::MemoryMap;
using Kernel::Mm::Page;
using Kernel::Mm::PageTable;
using Kernel::Mm::Pte;
using Kernel::Mm::PtePage;

namespace
{

const ulong PageBytes = Const::PageSize;
const ulong WindowSlots = PageTable::TmpMapPageCount;

/* ---- the machine ---- */

struct Machine
{
    uint8_t* Phys = nullptr;
    ulong Pages = 0;
    /* The TmpMap window and the L1 table whose entries map it */
    uint8_t* Window = nullptr;
    PtePage* WindowL1 = nullptr;
    /* What each slot holds: the physical address loaded into it, or none */
    long SlotPhys[WindowSlots];
    Page* PageArray = nullptr;
};

Machine M;

const long NoPhys = -1;

bool InRam(ulong phys)
{
    return phys / PageBytes < M.Pages;
}

void WriteBack(size_t slot)
{
    if (M.SlotPhys[slot] == NoPhys)
        return;
    ulong phys = static_cast<ulong>(M.SlotPhys[slot]);
    if (InRam(phys))
        memcpy(M.Phys + phys, M.Window + slot * PageBytes, PageBytes);
}

void WriteBackAll()
{
    for (size_t i = 0; i < WindowSlots; i++)
        WriteBack(i);
}

/* Slot i as its entry says now: what it held written back, what it maps
   loaded. */
void Sync(size_t i)
{
    uint8_t* slot = M.Window + i * PageBytes;
    WriteBack(i);
    Pte pte = M.WindowL1->Entry[i];
    if (!pte.Present())
    {
        M.SlotPhys[i] = NoPhys;
        ASAN_POISON_MEMORY_REGION(slot, PageBytes);
        return;
    }
    ulong phys = pte.Address();
    for (size_t j = 0; j < WindowSlots; j++)
    {
        INVARIANT(j == i || M.SlotPhys[j] != static_cast<long>(phys),
            "physical page 0x%lx mapped in TmpMap slots %zu and %zu at once", phys, j, i);
    }
    ASAN_UNPOISON_MEMORY_REGION(slot, PageBytes);
    if (InRam(phys))
        memcpy(slot, M.Phys + phys, PageBytes);
    else
        memset(slot, 0, PageBytes); /* a device's page: nothing to model */
    M.SlotPhys[i] = static_cast<long>(phys);
}

void SyncChanged()
{
    for (size_t i = 0; i < WindowSlots; i++)
    {
        Pte pte = M.WindowL1->Entry[i];
        long now = pte.Present() ? static_cast<long>(pte.Address()) : NoPhys;
        if (now != M.SlotPhys[i])
            Sync(i);
    }
}

}

/* ---- the MMU, where the kernel tells it something ---- */

namespace Fuzz
{

void HostTlbFlushPage(ulong virtAddr)
{
    if (M.Window == nullptr)
        return;
    ulong start = reinterpret_cast<ulong>(M.Window);
    if (virtAddr >= start && virtAddr < start + WindowSlots * PageBytes)
        Sync((virtAddr - start) / PageBytes);
}

void HostTlbFlushAll()
{
    if (M.Window != nullptr)
        SyncChanged();
}

void HostPteMadeValid()
{
    if (M.Window != nullptr)
        SyncChanged();
}

}

/* ---- the rest of the kernel page_table.cpp calls ---- */

namespace Kernel
{

Task* PreemptDisableTask()
{
    return nullptr;
}

void PreemptEnableTask(Task* task)
{
    (void)task;
}

}

namespace Hal
{

ulong MmioPremappedVa(ulong physAddr, ulong sizeBytes)
{
    (void)physAddr;
    (void)sizeBytes;
    return 0;
}

bool WriteCombining;

bool IsWriteCombiningAvailable()
{
    return WriteCombining;
}

}

namespace
{

/* ---- the model ---- */

struct PageState
{
    enum Kind
    {
        Machine, /* the kernel image, the firmware: never the allocator's */
        Free,
        Held,    /* the sequence's */
        Table,   /* a page table's, the kernel's own */
    };
    Kind State;
    long Ref;
    bool Frame;
};

std::vector<PageState> Pages;
/* va -> phys of the sequence's mappings, and of MMIO */
std::map<ulong, ulong> Maps;
std::map<ulong, ulong> Mmio;
/* The sequence's temp mappings: va -> (phys, what it wrote there) */
struct Tmp
{
    ulong Phys;
    ulong Pages;
    uint8_t Mark;
};
std::map<ulong, Tmp> Tmps;
size_t TmpSlots;
/* Leaves whose protection the sequence set: va -> (writable, executable) */
std::map<ulong, std::pair<bool, bool>> Prot;

const ulong ReservedPages = 8;

PageTable& Pt()
{
    return PageTable::GetInstance();
}

ulong Index(ulong phys)
{
    return phys / PageBytes;
}

Page* PageOf(ulong phys)
{
    return &M.PageArray[Index(phys)];
}

/* The kernel's own tables, found the way the MMU finds them: from the root,
   through physical memory. */
void Tables(std::set<ulong>& tables)
{
    WriteBackAll();
    tables.clear();
    std::vector<std::pair<ulong, int>> todo = {{Pt().Root, 4}};
    while (!todo.empty())
    {
        auto [phys, level] = todo.back();
        todo.pop_back();
        INVARIANT(InRam(phys), "a table at 0x%lx, past memory", phys);
        INVARIANT(tables.insert(phys).second, "table 0x%lx reached twice", phys);
        if (level == 1)
            continue;
        const PtePage* t = reinterpret_cast<const PtePage*>(M.Phys + phys);
        for (size_t i = 0; i < 512; i++)
        {
            Pte e = t->Entry[i];
            if (e.Present() && !e.Huge())
                todo.push_back({e.Address(), level - 1});
        }
    }
}

/* An entry's permissions, in either arch's encoding: what making it
   read-only or never-executable would change */
bool Writable(Pte p)
{
    Pte c = p;
    c.SetReadOnly();
    return c.Value != p.Value;
}

bool Executable(Pte p)
{
    Pte c = p;
    c.SetNoExecute();
    return c.Value != p.Value;
}

/* The leaf entry that maps va, read the way the MMU reads it; Value 0 if
   none. */
Pte Leaf(ulong va)
{
    WriteBackAll();
    Pte none;
    none.Value = 0;
    ulong phys = Pt().Root;
    const ulong idx[] = {Pte::L4Index(va), Pte::L3Index(va), Pte::L2Index(va), Pte::L1Index(va)};
    for (int level = 0; level < 4; level++)
    {
        const PtePage* t = reinterpret_cast<const PtePage*>(M.Phys + phys);
        Pte e = t->Entry[idx[level]];
        if (!e.Present())
            return none;
        if (level == 3)
            return e;
        phys = e.Address();
    }
    return none;
}

/* Everything the model says, against the machine. */
void Check(const char* after)
{
    /* Every page on the free list is a free one, once, and the count is
       the list's length */
    ulong onList = 0;
    std::set<ulong> free;
    for (Stdlib::ListEntry* e = Pt().FreePagesList.Flink; e != &Pt().FreePagesList; e = e->Flink)
    {
        Page* p = CONTAINING_RECORD(e, Page, ListEntry);
        ulong i = static_cast<ulong>(p - M.PageArray);
        INVARIANT(i < M.Pages, "the free list holds a page that is no page, after %s", after);
        INVARIANT(free.insert(i).second, "page %lu twice on the free list, after %s", i, after);
        onList++;
        INVARIANT(onList <= M.Pages, "the free list loops, after %s", after);
    }
    INVARIANT(Pt().FreePagesCount == onList, "FreePagesCount %lu with %lu on the list, after %s",
        Pt().FreePagesCount, onList, after);

    /* Tables the walk from the root finds are pages that left the free list
       for the kernel itself: the model learns of them here */
    std::set<ulong> tables;
    Tables(tables);
    for (ulong phys : tables)
    {
        PageState& s = Pages[Index(phys)];
        if (s.State == PageState::Free)
        {
            s.State = PageState::Table;
            Fuzz::Reached("a table allocated on the way down");
        }
        INVARIANT(s.State == PageState::Table, "page 0x%lx is a table, and %s's too (state %d), after %s", phys,
            "the sequence", s.State, after);
    }

    for (ulong i = 0; i < M.Pages; i++)
    {
        const PageState& s = Pages[i];
        Page& p = M.PageArray[i];
        INVARIANT(p.GetPhyAddress() == i * PageBytes, "page %lu's descriptor says 0x%lx", i, p.GetPhyAddress());
        if (s.State == PageState::Free)
            INVARIANT(free.count(i), "page %lu is free and not on the free list, after %s", i, after);
        else
            INVARIANT(!free.count(i), "page %lu (state %d) is on the free list, after %s", i, s.State, after);
        if (s.State == PageState::Table)
            INVARIANT(tables.count(i * PageBytes), "table page %lu is in no table, after %s", i, after);
        INVARIANT(p.RefCount.Get() == s.Ref, "page %lu's reference count is %ld where the rules make it %ld, "
            "after %s", i, p.RefCount.Get(), s.Ref, after);
    }

    /* No TmpMap slot kept by anything but the sequence */
    size_t used = 0;
    for (size_t i = 0; i < PageTable::TmpMapSharedCount; i++)
        used += Pt().TmpMapPageArray[i] != nullptr;
    INVARIANT(used == TmpSlots, "%zu TmpMap slots in use where the sequence holds %zu, after %s", used, TmpSlots,
        after);
    for (size_t i = PageTable::TmpMapSharedCount; i < WindowSlots; i++)
        INVARIANT(!M.WindowL1->Entry[i].Present(), "frame slot %zu left mapped, after %s",
            i - PageTable::TmpMapSharedCount, after);
    Fuzz::CheckNoLocksHeld(after);
}

/* The translations the model holds, as VirtToPhys and the leaves say. */
void CheckMaps(Fuzz::Input& in, const char* after)
{
    int n = 0;
    for (const auto& m : Maps)
    {
        if (n++ > 16 && !in.Chance(8))
            continue;
        ulong got = Pt().VirtToPhys(m.first + 8);
        INVARIANT(got == m.second + 8, "VirtToPhys(0x%lx) = 0x%lx where 0x%lx is mapped, after %s", m.first + 8,
            got, m.second, after);
        Pte leaf = Leaf(m.first);
        auto prot = Prot.find(m.first);
        bool w = (prot == Prot.end()) ? true : prot->second.first;
        bool x = (prot == Prot.end()) ? false : prot->second.second;
        INVARIANT(leaf.Present() && Writable(leaf) == w && Executable(leaf) == x,
            "the leaf of 0x%lx is %s%s, not %s%s, after %s", m.first, Writable(leaf) ? "w" : "",
            Executable(leaf) ? "x" : "", w ? "w" : "", x ? "x" : "", after);
    }
    for (const auto& m : Mmio)
        INVARIANT(Pt().VirtToPhys(m.first) == m.second, "the MMIO mapping of 0x%lx lost, after %s", m.second, after);
}

/* ---- the sequence's choices ---- */

std::vector<ulong> HeldPages()
{
    std::vector<ulong> v;
    for (ulong i = 0; i < M.Pages; i++)
    {
        if (Pages[i].State == PageState::Held)
            v.push_back(i * PageBytes);
    }
    return v;
}

/* A page the sequence holds and has not mapped, or 0 */
ulong Unmapped(Fuzz::Input& in)
{
    std::vector<ulong> v;
    for (ulong phys : HeldPages())
    {
        if (Pages[Index(phys)].Ref == 1 && !Pages[Index(phys)].Frame)
            v.push_back(phys);
    }
    return v.empty() ? 0 : v[in.Below(v.size())];
}

/* A kernel VA: runs in a few regions, so tables are shared, and runs that
   cross an L1 table's end and an L2's. */
ulong Va(Fuzz::Input& in)
{
    static const ulong Regions[] = {0xFFFF900000000000UL, 0xFFFF9000001FF000UL, 0xFFFF90003FFFE000UL,
                                    0xFFFFA00000000000UL, 0x0000100000000000UL};
    return in.Pick(Regions) + in.Below(64) * PageBytes;
}

bool RangeFree(ulong va, ulong count)
{
    for (ulong i = 0; i < count; i++)
    {
        if (Maps.count(va + i * PageBytes) || Mmio.count(va + i * PageBytes))
            return false;
    }
    return true;
}

bool RangeMapped(ulong va, ulong count)
{
    for (ulong i = 0; i < count; i++)
    {
        if (!Maps.count(va + i * PageBytes))
            return false;
    }
    return true;
}

/* A page handed out: it reads as zeros. */
void CheckZeroed(ulong phys, const char* by)
{
    WriteBackAll();
    for (ulong b = 0; b < PageBytes; b++)
        INVARIANT(M.Phys[phys + b] == 0, "page 0x%lx handed out by %s is not zeroed at +%lu", phys, by, b);
}

void Alloc(Fuzz::Input& in)
{
    (void)in;
    Page* p = Pt().AllocPage();
    if (p == nullptr)
    {
        INVARIANT(Pt().FreePagesCount == 0, "AllocPage failed with %lu pages free", Pt().FreePagesCount);
        Fuzz::Reached("memory exhausted");
        return;
    }
    ulong phys = p->GetPhyAddress();
    PageState& s = Pages[Index(phys)];
    INVARIANT(s.State == PageState::Free, "AllocPage handed out page 0x%lx, which is not free (state %d)", phys,
        s.State);
    s.State = PageState::Held;
    CheckZeroed(phys, "AllocPage");
}

void AllocRun(Fuzz::Input& in)
{
    ulong count = in.Chance(16) ? in.Range(129, 200) : 1 + in.Below(12);
    Page* first = Pt().AllocContiguousPages(count);
    if (first == nullptr)
    {
        Fuzz::Reached("no contiguous run");
        return;
    }
    INVARIANT(count <= PageTable::MaxContiguousPages, "a run of %lu pages, past the most", count);
    ulong base = first->GetPhyAddress();
    for (ulong i = 0; i < count; i++)
    {
        ulong phys = base + i * PageBytes;
        INVARIANT(InRam(phys) && &first[i] == PageOf(phys), "run page %lu is no page", i);
        PageState& s = Pages[Index(phys)];
        INVARIANT(s.State == PageState::Free, "AllocContiguousPages handed out page 0x%lx, not free", phys);
        s.State = PageState::Held;
        CheckZeroed(phys, "AllocContiguousPages");
    }
    Fuzz::Reached("a contiguous run");
}

/* Memory taken down to its last few pages: what makes a table allocation
   fail half way down a walk. */
void Exhaust(Fuzz::Input& in)
{
    ulong leave = in.Below(4);
    while (Pt().FreePagesCount > leave)
    {
        Page* p = Pt().AllocPage();
        INVARIANT(p != nullptr, "AllocPage failed with %lu pages free", Pt().FreePagesCount);
        Pages[Index(p->GetPhyAddress())].State = PageState::Held;
    }
    Fuzz::Reached("memory taken to its last pages");
}

void Free(Fuzz::Input& in)
{
    ulong phys = Unmapped(in);
    if (phys == 0)
        return;
    PageState& s = Pages[Index(phys)];
    if (in.Bool())
    {
        INVARIANT(Pt().IsFrameAddress(phys), "IsFrameAddress(0x%lx) says no", phys);
        Pt().FreeFrame(phys);
    }
    else
    {
        Pt().FreePage(PageOf(phys));
    }
    s.State = PageState::Free;
}

void Map(Fuzz::Input& in)
{
    std::vector<ulong> held = HeldPages();
    if (held.empty())
        return;
    ulong count = 1 + in.Below(in.Chance(32) ? 40 : 4);
    ulong va = Va(in);
    int form = in.U8() % 4;
    std::vector<Page*> pages;
    std::vector<ulong> phys;
    if (form == 2)
    {
        /* A contiguous run of the sequence's, as its descriptors */
        ulong start = held[in.Below(held.size())];
        for (ulong i = 0; i < count && InRam(start + i * PageBytes) &&
             Pages[Index(start + i * PageBytes)].State == PageState::Held; i++)
            phys.push_back(start + i * PageBytes);
        count = phys.size();
        for (ulong p : phys)
            pages.push_back(PageOf(p));
    }
    else
    {
        for (ulong i = 0; i < count; i++)
        {
            phys.push_back(held[in.Below(held.size())]);
            pages.push_back(PageOf(phys.back()));
        }
    }
    if (form == 0)
        count = 1;
    bool fits = RangeFree(va, count);

    bool ok;
    switch (form)
    {
    case 0:
        ok = Pt().MapPage(va, pages[0]);
        break;
    case 1:
        ok = Pt().MapPages(va, pages.data(), count);
        break;
    case 2:
        ok = Pt().MapContiguousPages(va, pages[0], count);
        break;
    default:
        ok = Pt().MapPhysPages(va, phys.data(), count);
        break;
    }
    Fuzz::Say("map %lu pages at 0x%lx (form %d): %d", count, va, form, ok);
    if (!fits)
        INVARIANT(!ok, "a map over 0x%lx, where something is mapped already, said yes", va);
    if (ok)
    {
        for (ulong i = 0; i < count; i++)
        {
            Maps[va + i * PageBytes] = phys[i];
            Pages[Index(phys[i])].Ref++;
            Prot.erase(va + i * PageBytes);
        }
        Fuzz::Reached("a map");
    }
    else if (fits)
    {
        Fuzz::Reached("a map that failed on the way");
    }
    /* Whatever happened, the translations are the model's: a map that
       failed left none of its own */
    for (ulong i = 0; i < count; i++)
    {
        ulong a = va + i * PageBytes;
        if (!Maps.count(a) && !Mmio.count(a))
            INVARIANT(Pt().VirtToPhys(a) == 0, "0x%lx is mapped after a map of it said %s", a, ok ? "yes" : "no");
    }
}

/* The TmpMap window all but full -- device pages, which take no page's
   reference -- and a map across an L1 table's end made in what is left.
   A walk holds two slots at most, a table and the next, and allocating a
   table on the way takes the second; with fewer the kernel says so and
   stops (ZeroPage's panic), which is what the window being full of leaked
   slots should do. With two, every map must go as it would with all. */
void Crowd(Fuzz::Input& in)
{
    std::vector<ulong> held = HeldPages();
    if (held.size() < 4)
        return;
    ulong keep = 2 + in.Below(4);
    ulong crowd = PageTable::TmpMapSharedCount - TmpSlots - keep;
    ulong device = (M.Pages + 4096) * PageBytes;
    ulong va = Pt().TmpMapRange(device, crowd * PageBytes);
    if (va == 0)
        return;
    TmpSlots += crowd;
    Fuzz::Reached("the TmpMap window crowded");

    /* Across the end of an L1 table, into a region no table maps yet */
    ulong count = 2 + in.Below(6);
    ulong at = 0xFFFFB00000000000UL + (512 - in.Below(count)) * PageBytes + in.Below(8) * (1UL << 30);
    std::vector<Page*> pages;
    std::vector<ulong> phys;
    for (ulong i = 0; i < count; i++)
    {
        phys.push_back(held[in.Below(held.size())]);
        pages.push_back(PageOf(phys.back()));
    }
    bool fits = RangeFree(at, count);
    bool ok = Pt().MapPages(at, pages.data(), count);
    for (ulong p = 0; p < crowd; p++)
        Pt().TmpUnmapPage(va + p * PageBytes);
    TmpSlots -= crowd;
    if (ok)
    {
        INVARIANT(fits, "a map over 0x%lx, where something is mapped already, said yes", at);
        for (ulong i = 0; i < count; i++)
        {
            Maps[at + i * PageBytes] = phys[i];
            Pages[Index(phys[i])].Ref++;
            Prot.erase(at + i * PageBytes);
        }
        Fuzz::Reached("a map in a crowded window");
        return;
    }
    Fuzz::Reached("a map a crowded window refused");
    if (fits && Pt().FreePagesCount >= 3)
        Fuzz::Reached("a map a crowded window refused, memory free");
    for (ulong i = 0; i < count; i++)
    {
        ulong a = at + i * PageBytes;
        if (!Maps.count(a) && !Mmio.count(a))
            INVARIANT(Pt().VirtToPhys(a) == 0, "0x%lx is mapped after a map of it in a crowded TmpMap window said no",
                a);
    }
}

void Unmap(Fuzz::Input& in)
{
    if (Maps.empty())
        return;
    auto it = Maps.begin();
    std::advance(it, in.Below(Maps.size()));
    ulong va = it->first;
    ulong count = 1;
    while (count < 8 && in.Bool() && Maps.count(va + count * PageBytes))
        count++;
    if (in.Chance(96) && count == 1)
    {
        /* The single-page form, and the caller's own Put after it */
        Page* p = Pt().UnmapPage(va);
        INVARIANT(p != nullptr && p->GetPhyAddress() == Maps[va], "UnmapPage(0x%lx) returned another page", va);
        p->Put();
        Pages[Index(Maps[va])].Ref--;
        Maps.erase(va);
        Prot.erase(va);
        Fuzz::Reached("UnmapPage");
        return;
    }
    /* Freeing through the unmap only pages mapped once, and there once */
    bool free = in.Bool();
    std::map<ulong, int> uses;
    for (ulong i = 0; i < count; i++)
        uses[Maps[va + i * PageBytes]]++;
    for (auto& u : uses)
    {
        if (u.second != 1 || Pages[Index(u.first)].Ref != 2 || Pages[Index(u.first)].Frame)
            free = false;
    }
    Pt().UnmapPages(va, count, free);
    for (ulong i = 0; i < count; i++)
    {
        ulong phys = Maps[va + i * PageBytes];
        Pages[Index(phys)].Ref--;
        if (free)
            Pages[Index(phys)].State = PageState::Free;
        Maps.erase(va + i * PageBytes);
        Prot.erase(va + i * PageBytes);
        INVARIANT(Pt().VirtToPhys(va + i * PageBytes) == 0, "0x%lx still mapped after its unmap", va + i * PageBytes);
    }
    Fuzz::Reached(free ? "an unmap that frees" : "an unmap");
}

void Protect(Fuzz::Input& in)
{
    if (Maps.empty())
        return;
    auto it = Maps.begin();
    std::advance(it, in.Below(Maps.size()));
    ulong va = it->first;
    ulong count = 1 + in.Below(4);
    bool w = in.Bool(), x = in.Bool() && !w, exact = in.Bool();
    bool ok = exact ? Pt().SetRangeProtection(va, count * PageBytes, w, x)
                    : Pt().ProtectRange(va, count * PageBytes, w, x);
    /* Page by page up to the first that is not mapped, which says no */
    ulong i = 0;
    for (; i < count && Maps.count(va + i * PageBytes); i++)
    {
        auto cur = Prot.count(va + i * PageBytes) ? Prot[va + i * PageBytes] : std::make_pair(true, false);
        if (exact)
            Prot[va + i * PageBytes] = {w, x};
        else
            Prot[va + i * PageBytes] = {cur.first && w, cur.second && x};
    }
    if (i < count && !RangeMapped(va + i * PageBytes, 1) && Pt().VirtToPhys(va + i * PageBytes) == 0)
        INVARIANT(!ok, "a protection change over 0x%lx, which is not mapped, said yes", va + i * PageBytes);
    Fuzz::Reached(ok ? "a protection change" : "a protection change that stopped");
}

void TmpMap(Fuzz::Input& in)
{
    if (!Tmps.empty() && (in.Bool() || Tmps.size() > 12))
    {
        /* One of the sequence's, unmapped: what it wrote is in the page */
        auto it = Tmps.begin();
        std::advance(it, in.Below(Tmps.size()));
        ulong va = it->first & ~(PageBytes - 1);
        Tmp t = it->second;
        for (ulong p = 0; p < t.Pages; p++)
        {
            ulong phys = Pt().TmpUnmapPage(va + p * PageBytes);
            INVARIANT(phys == t.Phys + p * PageBytes, "TmpUnmapPage gave back 0x%lx, not 0x%lx", phys,
                t.Phys + p * PageBytes);
            Pages[Index(phys)].Ref--;
        }
        TmpSlots -= t.Pages;
        Tmps.erase(it);
        WriteBackAll();
        for (ulong b = 0; b < t.Pages * PageBytes; b += 509)
            INVARIANT(M.Phys[t.Phys + b] == t.Mark, "a write through the TmpMap window to 0x%lx is not in the page",
                t.Phys + b);
        return;
    }
    ulong phys = Unmapped(in);
    if (phys == 0)
        return;
    /* The pages after it, while they are the sequence's and mapped nowhere
       either -- a page in two slots at once is nothing the kernel does */
    ulong pages = 1;
    while (pages < 3 && InRam(phys + pages * PageBytes) && in.Bool())
    {
        const PageState& next = Pages[Index(phys + pages * PageBytes)];
        if (next.State != PageState::Held || next.Ref != 1 || next.Frame)
            break;
        pages++;
    }
    ulong offset = in.Below(PageBytes);
    ulong len = (pages - 1) * PageBytes + 1 + in.Below(PageBytes - offset);
    ulong va = (pages == 1 && in.Bool()) ? Pt().TmpMapAddress(phys + offset) : Pt().TmpMapRange(phys + offset, len);
    if (va == 0)
    {
        INVARIANT(TmpSlots + pages > PageTable::TmpMapSharedCount - 16,
            "no TmpMap slot for %lu pages with %zu of the window's in use", pages, TmpSlots);
        return;
    }
    INVARIANT((va & (PageBytes - 1)) == offset, "a TmpMap of 0x%lx at 0x%lx: the offset is lost", phys + offset, va);
    ulong span = (offset + len + PageBytes - 1) / PageBytes;
    uint8_t mark = static_cast<uint8_t>(1 + in.Below(255));
    memset(reinterpret_cast<void*>(va - offset), mark, span * PageBytes);
    for (ulong p = 0; p < span; p++)
        Pages[Index(phys + p * PageBytes)].Ref++;
    TmpSlots += span;
    Tmps[va] = {phys, span, mark};
    Fuzz::Reached(span > 1 ? "a TmpMap of a range" : "a TmpMap");
}

void Frame(Fuzz::Input& in)
{
    ulong phys = Unmapped(in);
    if (phys == 0)
        return;
    Pages[Index(phys)].Frame = true;
    Fuzz::CurrentCpu = in.Below(64);
    ulong flags = Hal::IrqSave();
    ulong va = Pt().MapFrameSlot(phys);
    uint8_t mark = in.U8();
    memset(reinterpret_cast<void*>(va), mark, PageBytes);
    Pt().UnmapFrameSlot(va);
    Hal::IrqRestore(flags);
    WriteBackAll();
    for (ulong b = 0; b < PageBytes; b += 509)
        INVARIANT(M.Phys[phys + b] == mark, "a copy through frame slot %lu is not in frame 0x%lx", Fuzz::CurrentCpu,
            phys);
    Pages[Index(phys)].Frame = false;
    Fuzz::Reached("a frame copied");
}

void MapMmio(Fuzz::Input& in)
{
    ulong pa = (M.Pages + in.Below(64)) * PageBytes;
    ulong va = pa + MemoryMap::KernelSpaceBase;
    if (Mmio.count(va) || Maps.count(va))
        return;
    Hal::WriteCombining = in.Bool();
    ulong got = Pt().MapMmioRegion(pa, 1 + in.Below(PageBytes), in.Bool() ? PageTable::MmioWriteCombining
                                                                          : PageTable::MmioUncached);
    if (got == 0)
    {
        Fuzz::Reached("an MMIO map that failed on the way");
        return;
    }
    INVARIANT(got == va, "MMIO at 0x%lx mapped at 0x%lx", pa, got);
    Mmio[va] = pa;
    Pte leaf = Leaf(va);
    INVARIANT(leaf.Present() && !Executable(leaf), "an MMIO mapping that can be executed");
    Fuzz::Reached("an MMIO map");
}

/* ---- a machine as Setup leaves one ---- */

void Boot(Fuzz::Input& in)
{
    M.Pages = 48 + in.Below(in.Chance(32) ? 960 : 160);
    void* mem = nullptr;
    INVARIANT(posix_memalign(&mem, PageBytes, M.Pages * PageBytes) == 0, "no host memory");
    M.Phys = static_cast<uint8_t*>(mem);
    /* What a page held before: not zeros */
    std::vector<uint8_t> noise = Fuzz::Noise(in.U32(), PageBytes);
    for (ulong i = 0; i < M.Pages; i++)
        memcpy(M.Phys + i * PageBytes, noise.data(), PageBytes);
    INVARIANT(posix_memalign(&mem, PageTable::HugePageSize, WindowSlots * PageBytes) == 0, "no host memory");
    M.Window = static_cast<uint8_t*>(mem);
    ASAN_POISON_MEMORY_REGION(M.Window, WindowSlots * PageBytes);
    INVARIANT(posix_memalign(&mem, PageBytes, PageBytes) == 0, "no host memory");
    M.WindowL1 = static_cast<PtePage*>(mem);
    memset(M.WindowL1, 0, PageBytes);
    for (size_t i = 0; i < WindowSlots; i++)
        M.SlotPhys[i] = NoPhys;
    M.PageArray = new Page[M.Pages];

    MemoryMap::GetInstance().AddRegion(0, M.Pages * PageBytes, MemoryMap::UsableRamType);

    PageTable& pt = Pt();
    pt.PageArray = M.PageArray;
    pt.PageArrayCount = M.Pages;
    pt.HighestPhyAddr = M.Pages * PageBytes;
    pt.TotalPagesCount = M.Pages;
    pt.TmpMapStart = reinterpret_cast<ulong>(M.Window);
    pt.TmpMapL1Page = M.WindowL1;
    Pages.assign(M.Pages, {PageState::Machine, 1, false});
    /* What DrainEarlyFreeList leaves: every usable page on the list, at
       reference count 1 */
    for (ulong i = M.Pages; i-- > ReservedPages;)
    {
        M.PageArray[i].Init(i * PageBytes);
        pt.FreePagesList.InsertHead(&M.PageArray[i].ListEntry);
        pt.FreePagesCount++;
        Pages[i].State = PageState::Free;
    }
    for (ulong i = 0; i < ReservedPages; i++)
        M.PageArray[i].Init(i * PageBytes);

    /* The root, as Setup allocates it */
    Page* root = pt.AllocPage();
    INVARIANT(root != nullptr, "no page for the root");
    pt.Root = root->GetPhyAddress();
    Pages[Index(pt.Root)].State = PageState::Table;
}

void Run(Fuzz::Input& in)
{
    Boot(in);
    Check("the boot");
    for (int ops = 0; ops < 64 && in.More(); ops++)
    {
        switch (in.U8() % 16)
        {
        case 0:
        case 1:
        case 2:
            Alloc(in);
            break;
        case 3:
            AllocRun(in);
            break;
        case 4:
            Free(in);
            break;
        case 5:
            if (in.Chance(48))
                Exhaust(in);
            else
                Free(in);
            break;
        case 6:
        case 7:
        case 8:
            Map(in);
            break;
        case 9:
        case 10:
            Unmap(in);
            break;
        case 11:
            Protect(in);
            break;
        case 12:
        case 13:
            TmpMap(in);
            break;
        case 14:
            if (in.Chance(64))
                Crowd(in);
            else
                Frame(in);
            break;
        default:
            MapMmio(in);
            break;
        }
        Check("an operation");
        if (in.Chance(64))
            CheckMaps(in, "an operation");
    }
    CheckMaps(in, "the sequence");
}

void Reset()
{
    Tmps.clear();
    TmpSlots = 0;
    Maps.clear();
    Mmio.clear();
    Prot.clear();
    Pages.clear();
    if (M.Window != nullptr)
    {
        ASAN_UNPOISON_MEMORY_REGION(M.Window, WindowSlots * PageBytes);
        free(M.Window);
        free(M.WindowL1);
        free(M.Phys);
        delete[] M.PageArray;
    }
    M = Machine();
    PageTable& pt = Pt();
    pt.~PageTable();
    new (&pt) PageTable();
    MemoryMap& mmap = MemoryMap::GetInstance();
    mmap.~MemoryMap();
    new (&mmap) MemoryMap();
}

}

const Fuzz::Target Fuzz::TheTarget = {"pagetable", Reset, Run, 1024, 20000};
