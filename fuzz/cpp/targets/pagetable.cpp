// The page tables and the physical page allocator (mm/page_table.cpp), past
// Setup: what every mapping, every page and every frame of the running
// kernel goes through, and the error paths nothing else takes -- a table
// that cannot be allocated half way down a walk, a tmp_map window with no slot
// left, a machine with no free page.
//
// The machine is a small one: physical memory is a host buffer, page_array a
// host array, and the state PageTable::setup leaves -- the root, the free
// list, the tmp_map window and the L1 table that maps it -- is set up here as
// Setup documents it, since Setup itself runs on the bootstrap linear map. The
// MMU is emulated where the code reaches memory through it, the tmp_map
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
// zeroed; a map that fails leaving nothing mapped; no tmp_map slot left behind
// by any path. A machine of more than 4 MiB may have a huge pool reserved
// (hugepages=), its 2MiB blocks handed out and given back: a block of it
// never on the free list, never another's, zeroed when handed out, and the
// pool's counts its bitmaps'.
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

using namespace kernel;
using kernel::mm::MemoryMap;
using kernel::mm::Page;
using kernel::mm::PageTable;
using kernel::mm::Pte;
using kernel::mm::PtePage;

namespace
{

const ulong PAGE_BYTES = consts::PAGE_SIZE;
const ulong WINDOW_SLOTS = PageTable::TMP_MAP_PAGE_COUNT;

/* ---- the machine ---- */

struct Machine
{
    uint8_t* phys = nullptr;
    ulong pages = 0;
    /* The tmp_map window and the L1 table whose entries map it */
    uint8_t* window = nullptr;
    PtePage* window_l1 = nullptr;
    /* What each slot holds: the physical address loaded into it, or none */
    long slot_phys[WINDOW_SLOTS];
    Page* page_array = nullptr;
};

Machine machine;

const long NO_PHYS = -1;

bool in_ram(ulong phys)
{
    return phys / PAGE_BYTES < machine.pages;
}

void write_back(size_t slot)
{
    if (machine.slot_phys[slot] == NO_PHYS)
        return;
    ulong phys = static_cast<ulong>(machine.slot_phys[slot]);
    if (in_ram(phys))
        memcpy(machine.phys + phys, machine.window + slot * PAGE_BYTES, PAGE_BYTES);
}

void write_back_all()
{
    for (size_t i = 0; i < WINDOW_SLOTS; i++)
        write_back(i);
}

/* Slot i as its entry says now: what it held written back, what it maps
   loaded. */
void sync(size_t i)
{
    uint8_t* slot = machine.window + i * PAGE_BYTES;
    write_back(i);
    Pte pte = machine.window_l1->entry[i];
    if (!pte.present())
    {
        machine.slot_phys[i] = NO_PHYS;
        ASAN_POISON_MEMORY_REGION(slot, PAGE_BYTES);
        return;
    }
    ulong phys = pte.address();
    for (size_t j = 0; j < WINDOW_SLOTS; j++)
    {
        INVARIANT(j == i || machine.slot_phys[j] != static_cast<long>(phys),
            "physical page 0x%lx mapped in tmp_map slots %zu and %zu at once", phys, j, i);
    }
    ASAN_UNPOISON_MEMORY_REGION(slot, PAGE_BYTES);
    if (in_ram(phys))
        memcpy(slot, machine.phys + phys, PAGE_BYTES);
    else
        memset(slot, 0, PAGE_BYTES); /* a device's page: nothing to model */
    machine.slot_phys[i] = static_cast<long>(phys);
}

void sync_changed()
{
    for (size_t i = 0; i < WINDOW_SLOTS; i++)
    {
        Pte pte = machine.window_l1->entry[i];
        long now = pte.present() ? static_cast<long>(pte.address()) : NO_PHYS;
        if (now != machine.slot_phys[i])
            sync(i);
    }
}

}

/* ---- the MMU, where the kernel tells it something ---- */

namespace fuzz
{

void host_tlb_flush_page(ulong virt_addr)
{
    if (machine.window == nullptr)
        return;
    ulong start = reinterpret_cast<ulong>(machine.window);
    if (virt_addr >= start && virt_addr < start + WINDOW_SLOTS * PAGE_BYTES)
        sync((virt_addr - start) / PAGE_BYTES);
}

void host_tlb_flush_all()
{
    if (machine.window != nullptr)
        sync_changed();
}

void host_pte_made_valid()
{
    if (machine.window != nullptr)
        sync_changed();
}

}

/* ---- the rest of the kernel page_table.cpp calls ---- */

namespace kernel
{

Task* preempt_disable_task()
{
    return nullptr;
}

void preempt_enable_task(Task* task)
{
    (void)task;
}

}

namespace hal
{

ulong mmio_premapped_va(ulong phys_addr, ulong size_bytes)
{
    (void)phys_addr;
    (void)size_bytes;
    return 0;
}

bool write_combining;

bool is_write_combining_available()
{
    return write_combining;
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
        Pool,    /* in a block of the huge pool, free there */
        Huge,    /* in a block of the huge pool the sequence holds */
    };
    Kind state;
    long ref;
    bool frame;
};

std::vector<PageState> pages;
/* va -> phys of the sequence's mappings, and of MMIO */
std::map<ulong, ulong> maps;
std::map<ulong, ulong> mmio;
/* The sequence's temp mappings: va -> (phys, what it wrote there) */
struct Tmp
{
    ulong phys;
    ulong pages;
    uint8_t mark;
};
std::map<ulong, Tmp> tmps;
size_t tmp_slots;
/* Leaves whose protection the sequence set: va -> (writable, executable) */
std::map<ulong, std::pair<bool, bool>> prot;
/* The pool's blocks, by their first page's address: whether the sequence
   holds each */
std::map<ulong, bool> pool_blocks;

const ulong RESERVED_PAGES = 8;
const ulong HUGE_PAGES = PageTable::HUGE_FRAME_PAGES;
const ulong HUGE_BYTES = PageTable::HUGE_FRAME_SIZE;

PageTable& tested_pt()
{
    return PageTable::get_instance();
}

ulong index(ulong phys)
{
    return phys / PAGE_BYTES;
}

Page* page_of(ulong phys)
{
    return &machine.page_array[index(phys)];
}

/* The kernel's own tables, found the way the MMU finds them: from the root,
   through physical memory. */
void collect_tables(std::set<ulong>& tables)
{
    write_back_all();
    tables.clear();
    std::vector<std::pair<ulong, int>> todo = {{tested_pt().root_, 4}};
    while (!todo.empty())
    {
        auto [phys, level] = todo.back();
        todo.pop_back();
        INVARIANT(in_ram(phys), "a table at 0x%lx, past memory", phys);
        INVARIANT(tables.insert(phys).second, "table 0x%lx reached twice", phys);
        if (level == 1)
            continue;
        const PtePage* t = reinterpret_cast<const PtePage*>(machine.phys + phys);
        for (size_t i = 0; i < 512; i++)
        {
            Pte e = t->entry[i];
            if (e.present() && !e.huge())
                todo.push_back({e.address(), level - 1});
        }
    }
}

/* An entry's permissions, in either arch's encoding: what making it
   read-only or never-executable would change */
bool writable(Pte p)
{
    Pte c = p;
    c.set_read_only();
    return c.value != p.value;
}

bool executable(Pte p)
{
    Pte c = p;
    c.set_no_execute();
    return c.value != p.value;
}

/* The leaf entry that maps va, read the way the MMU reads it; Value 0 if
   none. */
Pte leaf_of(ulong va)
{
    write_back_all();
    Pte none;
    none.value = 0;
    ulong phys = tested_pt().root_;
    const ulong idx[] = {Pte::l4_index(va), Pte::l3_index(va), Pte::l2_index(va), Pte::l1_index(va)};
    for (int level = 0; level < 4; level++)
    {
        const PtePage* t = reinterpret_cast<const PtePage*>(machine.phys + phys);
        Pte e = t->entry[idx[level]];
        if (!e.present())
            return none;
        if (level == 3)
            return e;
        phys = e.address();
    }
    return none;
}

/* Everything the model says, against the machine. */
void check(const char* after)
{
    /* Every page on the free list is a free one, once, and the count is
       the list's length */
    ulong on_list = 0;
    std::set<ulong> free;
    for (stdlib::ListEntry* e = tested_pt().free_pages_list_.flink; e != &tested_pt().free_pages_list_; e = e->flink)
    {
        Page* p = CONTAINING_RECORD(e, Page, list_entry);
        ulong i = static_cast<ulong>(p - machine.page_array);
        INVARIANT(i < machine.pages, "the free list holds a page that is no page, after %s", after);
        INVARIANT(free.insert(i).second, "page %lu twice on the free list, after %s", i, after);
        on_list++;
        INVARIANT(on_list <= machine.pages, "the free list loops, after %s", after);
    }
    INVARIANT(tested_pt().free_pages_count_ == on_list, "free_pages_count_ %lu with %lu on the list, after %s",
        tested_pt().free_pages_count_, on_list, after);

    /* The pool as the model has it, in its counts and its bitmaps */
    ulong pool_free = 0;
    for (const auto& b : pool_blocks)
    {
        const ulong block = b.first / HUGE_BYTES;
        INVARIANT(tested_pt().huge_pool_test(tested_pt().huge_pool_in_, block), "block 0x%lx not in the pool's bitmap, after %s", b.first,
            after);
        INVARIANT(tested_pt().huge_pool_test(tested_pt().huge_pool_free_bits_, block) == !b.second, "block 0x%lx %s by the bitmap, after %s",
            b.first, b.second ? "free" : "held", after);
        pool_free += !b.second;
    }
    INVARIANT(tested_pt().get_huge_pool_blocks() == pool_blocks.size() && tested_pt().get_huge_pool_free() == pool_free,
        "the pool says %lu blocks, %lu free, where it has %zu, %lu free, after %s", tested_pt().get_huge_pool_blocks(),
        tested_pt().get_huge_pool_free(), pool_blocks.size(), pool_free, after);

    /* Tables the walk from the root finds are pages that left the free list
       for the kernel itself: the model learns of them here */
    std::set<ulong> tables;
    collect_tables(tables);
    for (ulong phys : tables)
    {
        PageState& s = pages[index(phys)];
        if (s.state == PageState::Free)
        {
            s.state = PageState::Table;
            fuzz::reached("a table allocated on the way down");
        }
        INVARIANT(s.state == PageState::Table, "page 0x%lx is a table, and %s's too (state %d), after %s", phys,
            "the sequence", s.state, after);
    }

    for (ulong i = 0; i < machine.pages; i++)
    {
        const PageState& s = pages[i];
        Page& p = machine.page_array[i];
        INVARIANT(p.get_phy_address() == i * PAGE_BYTES, "page %lu's descriptor says 0x%lx", i, p.get_phy_address());
        if (s.state == PageState::Free)
            INVARIANT(free.count(i), "page %lu is free and not on the free list, after %s", i, after);
        else
            INVARIANT(!free.count(i), "page %lu (state %d) is on the free list, after %s", i, s.state, after);
        if (s.state == PageState::Table)
            INVARIANT(tables.count(i * PAGE_BYTES), "table page %lu is in no table, after %s", i, after);
        INVARIANT(p.ref_count.get() == s.ref, "page %lu's reference count is %ld where the rules make it %ld, "
            "after %s", i, p.ref_count.get(), s.ref, after);
    }

    /* No tmp_map slot kept by anything but the sequence */
    size_t used = 0;
    for (size_t i = 0; i < PageTable::TMP_MAP_SHARED_COUNT; i++)
        used += tested_pt().tmp_map_page_array_[i] != nullptr;
    INVARIANT(used == tmp_slots, "%zu tmp_map slots in use where the sequence holds %zu, after %s", used, tmp_slots,
        after);
    for (size_t i = PageTable::TMP_MAP_SHARED_COUNT; i < WINDOW_SLOTS; i++)
        INVARIANT(!machine.window_l1->entry[i].present(), "frame slot %zu left mapped, after %s",
            i - PageTable::TMP_MAP_SHARED_COUNT, after);
    fuzz::check_no_locks_held(after);
}

/* The translations the model holds, as virt_to_phys and the leaves say. */
void check_maps(fuzz::Input& in, const char* after)
{
    int n = 0;
    for (const auto& m : maps)
    {
        if (n++ > 16 && !in.chance(8))
            continue;
        ulong got = tested_pt().virt_to_phys(m.first + 8);
        INVARIANT(got == m.second + 8, "virt_to_phys(0x%lx) = 0x%lx where 0x%lx is mapped, after %s", m.first + 8,
            got, m.second, after);
        Pte leaf = leaf_of(m.first);
        auto found = prot.find(m.first);
        bool w = (found == prot.end()) ? true : found->second.first;
        bool x = (found == prot.end()) ? false : found->second.second;
        INVARIANT(leaf.present() && writable(leaf) == w && executable(leaf) == x,
            "the leaf of 0x%lx is %s%s, not %s%s, after %s", m.first, writable(leaf) ? "w" : "",
            executable(leaf) ? "x" : "", w ? "w" : "", x ? "x" : "", after);
    }
    for (const auto& m : mmio)
        INVARIANT(tested_pt().virt_to_phys(m.first) == m.second, "the MMIO mapping of 0x%lx lost, after %s", m.second, after);
}

/* ---- the sequence's choices ---- */

std::vector<ulong> held_pages()
{
    std::vector<ulong> v;
    for (ulong i = 0; i < machine.pages; i++)
    {
        if (pages[i].state == PageState::Held)
            v.push_back(i * PAGE_BYTES);
    }
    return v;
}

/* A page the sequence holds and has not mapped, or 0 */
ulong unmapped(fuzz::Input& in)
{
    std::vector<ulong> v;
    for (ulong phys : held_pages())
    {
        if (pages[index(phys)].ref == 1 && !pages[index(phys)].frame)
            v.push_back(phys);
    }
    return v.empty() ? 0 : v[in.below(v.size())];
}

/* A kernel VA: runs in a few regions, so tables are shared, and runs that
   cross an L1 table's end and an L2's. */
ulong pick_va(fuzz::Input& in)
{
    static const ulong REGIONS[] = {0xFFFF900000000000UL, 0xFFFF9000001FF000UL, 0xFFFF90003FFFE000UL,
                                    0xFFFFA00000000000UL, 0x0000100000000000UL};
    return in.pick(REGIONS) + in.below(64) * PAGE_BYTES;
}

bool range_free(ulong va, ulong count)
{
    for (ulong i = 0; i < count; i++)
    {
        if (maps.count(va + i * PAGE_BYTES) || mmio.count(va + i * PAGE_BYTES))
            return false;
    }
    return true;
}

bool range_mapped(ulong va, ulong count)
{
    for (ulong i = 0; i < count; i++)
    {
        if (!maps.count(va + i * PAGE_BYTES))
            return false;
    }
    return true;
}

/* A page handed out: it reads as zeros. */
void check_zeroed(ulong phys, const char* by)
{
    write_back_all();
    for (ulong b = 0; b < PAGE_BYTES; b++)
        INVARIANT(machine.phys[phys + b] == 0, "page 0x%lx handed out by %s is not zeroed at +%lu", phys, by, b);
}

void alloc(fuzz::Input& in)
{
    (void)in;
    Page* p = tested_pt().alloc_page();
    if (p == nullptr)
    {
        INVARIANT(tested_pt().free_pages_count_ == 0, "alloc_page failed with %lu pages free", tested_pt().free_pages_count_);
        fuzz::reached("memory exhausted");
        return;
    }
    ulong phys = p->get_phy_address();
    PageState& s = pages[index(phys)];
    INVARIANT(s.state == PageState::Free, "alloc_page handed out page 0x%lx, which is not free (state %d)", phys,
        s.state);
    s.state = PageState::Held;
    check_zeroed(phys, "alloc_page");
}

void alloc_run(fuzz::Input& in)
{
    ulong count = in.chance(16) ? in.range(129, 200) : 1 + in.below(12);
    Page* first = tested_pt().alloc_contiguous_pages(count);
    if (first == nullptr)
    {
        fuzz::reached("no contiguous run");
        return;
    }
    INVARIANT(count <= PageTable::MAX_CONTIGUOUS_PAGES, "a run of %lu pages, past the most", count);
    ulong base = first->get_phy_address();
    for (ulong i = 0; i < count; i++)
    {
        ulong phys = base + i * PAGE_BYTES;
        INVARIANT(in_ram(phys) && &first[i] == page_of(phys), "run page %lu is no page", i);
        PageState& s = pages[index(phys)];
        INVARIANT(s.state == PageState::Free, "alloc_contiguous_pages handed out page 0x%lx, not free", phys);
        s.state = PageState::Held;
        check_zeroed(phys, "alloc_contiguous_pages");
    }
    fuzz::reached("a contiguous run");
}

/* Memory taken down to its last few pages: what makes a table allocation
   fail half way down a walk. */
void exhaust(fuzz::Input& in)
{
    ulong leave = in.below(4);
    while (tested_pt().free_pages_count_ > leave)
    {
        Page* p = tested_pt().alloc_page();
        INVARIANT(p != nullptr, "alloc_page failed with %lu pages free", tested_pt().free_pages_count_);
        pages[index(p->get_phy_address())].state = PageState::Held;
    }
    fuzz::reached("memory taken to its last pages");
}

void free_one(fuzz::Input& in)
{
    ulong phys = unmapped(in);
    if (phys == 0)
        return;
    PageState& s = pages[index(phys)];
    if (in.boolean())
    {
        INVARIANT(tested_pt().is_frame_address(phys), "is_frame_address(0x%lx) says no", phys);
        tested_pt().free_frame(phys);
    }
    else
    {
        tested_pt().free_page(page_of(phys));
    }
    s.state = PageState::Free;
}

void map(fuzz::Input& in)
{
    std::vector<ulong> held = held_pages();
    if (held.empty())
        return;
    ulong count = 1 + in.below(in.chance(32) ? 40 : 4);
    ulong va = pick_va(in);
    int form = in.u8() % 4;
    std::vector<Page*> page_ptrs;
    std::vector<ulong> phys;
    if (form == 2)
    {
        /* A contiguous run of the sequence's, as its descriptors */
        ulong start = held[in.below(held.size())];
        for (ulong i = 0; i < count && in_ram(start + i * PAGE_BYTES) &&
             pages[index(start + i * PAGE_BYTES)].state == PageState::Held; i++)
            phys.push_back(start + i * PAGE_BYTES);
        count = phys.size();
        for (ulong p : phys)
            page_ptrs.push_back(page_of(p));
    }
    else
    {
        for (ulong i = 0; i < count; i++)
        {
            phys.push_back(held[in.below(held.size())]);
            page_ptrs.push_back(page_of(phys.back()));
        }
    }
    if (form == 0)
        count = 1;
    bool fits = range_free(va, count);

    bool ok;
    switch (form)
    {
    case 0:
        ok = tested_pt().map_page(va, page_ptrs[0]);
        break;
    case 1:
        ok = tested_pt().map_pages(va, page_ptrs.data(), count);
        break;
    case 2:
        ok = tested_pt().map_contiguous_pages(va, page_ptrs[0], count);
        break;
    default:
        ok = tested_pt().map_phys_pages(va, phys.data(), count);
        break;
    }
    fuzz::say("map %lu pages at 0x%lx (form %d): %d", count, va, form, ok);
    if (!fits)
        INVARIANT(!ok, "a map over 0x%lx, where something is mapped already, said yes", va);
    if (ok)
    {
        for (ulong i = 0; i < count; i++)
        {
            maps[va + i * PAGE_BYTES] = phys[i];
            pages[index(phys[i])].ref++;
            prot.erase(va + i * PAGE_BYTES);
        }
        fuzz::reached("a map");
    }
    else if (fits)
    {
        fuzz::reached("a map that failed on the way");
    }
    /* Whatever happened, the translations are the model's: a map that
       failed left none of its own */
    for (ulong i = 0; i < count; i++)
    {
        ulong a = va + i * PAGE_BYTES;
        if (!maps.count(a) && !mmio.count(a))
            INVARIANT(tested_pt().virt_to_phys(a) == 0, "0x%lx is mapped after a map of it said %s", a, ok ? "yes" : "no");
    }
}

/* The tmp_map window all but full -- device pages, which take no page's
   reference -- and a map across an L1 table's end made in what is left.
   A walk holds two slots at most, a table and the next, and allocating a
   table on the way takes the second; with fewer the kernel says so and
   stops (zero_page's panic), which is what the window being full of leaked
   slots should do. With two, every map must go as it would with all. */
void crowd(fuzz::Input& in)
{
    std::vector<ulong> held = held_pages();
    if (held.size() < 4)
        return;
    ulong keep = 2 + in.below(4);
    ulong crowd = PageTable::TMP_MAP_SHARED_COUNT - tmp_slots - keep;
    ulong device = (machine.pages + 4096) * PAGE_BYTES;
    ulong va = tested_pt().tmp_map_range(device, crowd * PAGE_BYTES);
    if (va == 0)
        return;
    tmp_slots += crowd;
    fuzz::reached("the tmp_map window crowded");

    /* Across the end of an L1 table, into a region no table maps yet */
    ulong count = 2 + in.below(6);
    ulong at = 0xFFFFB00000000000UL + (512 - in.below(count)) * PAGE_BYTES + in.below(8) * (1UL << 30);
    std::vector<Page*> page_ptrs;
    std::vector<ulong> phys;
    for (ulong i = 0; i < count; i++)
    {
        phys.push_back(held[in.below(held.size())]);
        page_ptrs.push_back(page_of(phys.back()));
    }
    bool fits = range_free(at, count);
    bool ok = tested_pt().map_pages(at, page_ptrs.data(), count);
    for (ulong p = 0; p < crowd; p++)
        tested_pt().tmp_unmap_page(va + p * PAGE_BYTES);
    tmp_slots -= crowd;
    if (ok)
    {
        INVARIANT(fits, "a map over 0x%lx, where something is mapped already, said yes", at);
        for (ulong i = 0; i < count; i++)
        {
            maps[at + i * PAGE_BYTES] = phys[i];
            pages[index(phys[i])].ref++;
            prot.erase(at + i * PAGE_BYTES);
        }
        fuzz::reached("a map in a crowded window");
        return;
    }
    fuzz::reached("a map a crowded window refused");
    if (fits && tested_pt().free_pages_count_ >= 3)
        fuzz::reached("a map a crowded window refused, memory free");
    for (ulong i = 0; i < count; i++)
    {
        ulong a = at + i * PAGE_BYTES;
        if (!maps.count(a) && !mmio.count(a))
            INVARIANT(tested_pt().virt_to_phys(a) == 0, "0x%lx is mapped after a map of it in a crowded tmp_map window said no",
                a);
    }
}

void unmap(fuzz::Input& in)
{
    if (maps.empty())
        return;
    auto it = maps.begin();
    std::advance(it, in.below(maps.size()));
    ulong va = it->first;
    ulong count = 1;
    while (count < 8 && in.boolean() && maps.count(va + count * PAGE_BYTES))
        count++;
    if (in.chance(96) && count == 1)
    {
        /* The single-page form, and the caller's own Put after it */
        Page* p = tested_pt().unmap_page(va);
        INVARIANT(p != nullptr && p->get_phy_address() == maps[va], "unmap_page(0x%lx) returned another page", va);
        p->put();
        pages[index(maps[va])].ref--;
        maps.erase(va);
        prot.erase(va);
        fuzz::reached("unmap_page");
        return;
    }
    /* Freeing through the unmap only pages mapped once, and there once */
    bool free = in.boolean();
    std::map<ulong, int> uses;
    for (ulong i = 0; i < count; i++)
        uses[maps[va + i * PAGE_BYTES]]++;
    for (auto& u : uses)
    {
        if (u.second != 1 || pages[index(u.first)].ref != 2 || pages[index(u.first)].frame)
            free = false;
    }
    tested_pt().unmap_pages(va, count, free);
    for (ulong i = 0; i < count; i++)
    {
        ulong phys = maps[va + i * PAGE_BYTES];
        pages[index(phys)].ref--;
        if (free)
            pages[index(phys)].state = PageState::Free;
        maps.erase(va + i * PAGE_BYTES);
        prot.erase(va + i * PAGE_BYTES);
        INVARIANT(tested_pt().virt_to_phys(va + i * PAGE_BYTES) == 0, "0x%lx still mapped after its unmap", va + i * PAGE_BYTES);
    }
    fuzz::reached(free ? "an unmap that frees" : "an unmap");
}

void protect(fuzz::Input& in)
{
    if (maps.empty())
        return;
    auto it = maps.begin();
    std::advance(it, in.below(maps.size()));
    ulong va = it->first;
    ulong count = 1 + in.below(4);
    bool w = in.boolean(), x = in.boolean() && !w, exact = in.boolean();
    bool ok = exact ? tested_pt().set_range_protection(va, count * PAGE_BYTES, w, x)
                    : tested_pt().protect_range(va, count * PAGE_BYTES, w, x);
    /* Page by page up to the first that is not mapped, which says no */
    ulong i = 0;
    for (; i < count && maps.count(va + i * PAGE_BYTES); i++)
    {
        auto cur = prot.count(va + i * PAGE_BYTES) ? prot[va + i * PAGE_BYTES] : std::make_pair(true, false);
        if (exact)
            prot[va + i * PAGE_BYTES] = {w, x};
        else
            prot[va + i * PAGE_BYTES] = {cur.first && w, cur.second && x};
    }
    if (i < count && !range_mapped(va + i * PAGE_BYTES, 1) && tested_pt().virt_to_phys(va + i * PAGE_BYTES) == 0)
        INVARIANT(!ok, "a protection change over 0x%lx, which is not mapped, said yes", va + i * PAGE_BYTES);
    fuzz::reached(ok ? "a protection change" : "a protection change that stopped");
}

void tmp_map(fuzz::Input& in)
{
    if (!tmps.empty() && (in.boolean() || tmps.size() > 12))
    {
        /* One of the sequence's, unmapped: what it wrote is in the page */
        auto it = tmps.begin();
        std::advance(it, in.below(tmps.size()));
        ulong va = it->first & ~(PAGE_BYTES - 1);
        Tmp t = it->second;
        for (ulong p = 0; p < t.pages; p++)
        {
            ulong phys = tested_pt().tmp_unmap_page(va + p * PAGE_BYTES);
            INVARIANT(phys == t.phys + p * PAGE_BYTES, "tmp_unmap_page gave back 0x%lx, not 0x%lx", phys,
                t.phys + p * PAGE_BYTES);
            pages[index(phys)].ref--;
        }
        tmp_slots -= t.pages;
        tmps.erase(it);
        write_back_all();
        for (ulong b = 0; b < t.pages * PAGE_BYTES; b += 509)
            INVARIANT(machine.phys[t.phys + b] == t.mark, "a write through the tmp_map window to 0x%lx is not in the page",
                t.phys + b);
        return;
    }
    ulong phys = unmapped(in);
    if (phys == 0)
        return;
    /* The pages after it, while they are the sequence's and mapped nowhere
       either -- a page in two slots at once is nothing the kernel does */
    ulong npages = 1;
    while (npages < 3 && in_ram(phys + npages * PAGE_BYTES) && in.boolean())
    {
        const PageState& next = pages[index(phys + npages * PAGE_BYTES)];
        if (next.state != PageState::Held || next.ref != 1 || next.frame)
            break;
        npages++;
    }
    ulong offset = in.below(PAGE_BYTES);
    ulong len = (npages - 1) * PAGE_BYTES + 1 + in.below(PAGE_BYTES - offset);
    ulong va = (npages == 1 && in.boolean()) ? tested_pt().tmp_map_address(phys + offset) : tested_pt().tmp_map_range(phys + offset, len);
    if (va == 0)
    {
        INVARIANT(tmp_slots + npages > PageTable::TMP_MAP_SHARED_COUNT - 16,
            "no tmp_map slot for %lu pages with %zu of the window's in use", npages, tmp_slots);
        return;
    }
    INVARIANT((va & (PAGE_BYTES - 1)) == offset, "a tmp_map of 0x%lx at 0x%lx: the offset is lost", phys + offset, va);
    ulong span = (offset + len + PAGE_BYTES - 1) / PAGE_BYTES;
    uint8_t mark = static_cast<uint8_t>(1 + in.below(255));
    memset(reinterpret_cast<void*>(va - offset), mark, span * PAGE_BYTES);
    for (ulong p = 0; p < span; p++)
        pages[index(phys + p * PAGE_BYTES)].ref++;
    tmp_slots += span;
    tmps[va] = {phys, span, mark};
    fuzz::reached(span > 1 ? "a tmp_map of a range" : "a tmp_map");
}

void frame(fuzz::Input& in)
{
    ulong phys = unmapped(in);
    if (phys == 0)
        return;
    pages[index(phys)].frame = true;
    fuzz::current_cpu = in.below(64);
    ulong flags = hal::irq_save();
    ulong va = tested_pt().map_frame_slot(phys);
    uint8_t mark = in.u8();
    memset(reinterpret_cast<void*>(va), mark, PAGE_BYTES);
    tested_pt().unmap_frame_slot(va);
    hal::irq_restore(flags);
    write_back_all();
    for (ulong b = 0; b < PAGE_BYTES; b += 509)
        INVARIANT(machine.phys[phys + b] == mark, "a copy through frame slot %lu is not in frame 0x%lx", fuzz::current_cpu,
            phys);
    pages[index(phys)].frame = false;
    fuzz::reached("a frame copied");
}

void map_mmio(fuzz::Input& in)
{
    ulong pa = (machine.pages + in.below(64)) * PAGE_BYTES;
    ulong va = pa + MemoryMap::KERNEL_SPACE_BASE;
    if (mmio.count(va) || maps.count(va))
        return;
    hal::write_combining = in.boolean();
    ulong got = tested_pt().map_mmio_region(pa, 1 + in.below(PAGE_BYTES), in.boolean() ? PageTable::MmioWriteCombining
                                                                          : PageTable::MmioUncached);
    if (got == 0)
    {
        fuzz::reached("an MMIO map that failed on the way");
        return;
    }
    INVARIANT(got == va, "MMIO at 0x%lx mapped at 0x%lx", pa, got);
    mmio[va] = pa;
    Pte leaf = leaf_of(va);
    INVARIANT(leaf.present() && !executable(leaf), "an MMIO mapping that can be executed");
    fuzz::reached("an MMIO map");
}

/* The huge pool, reserved as the boot reserves it: whole free blocks, the
   highest first, leaving keep pages free. */
void reserve_pool(fuzz::Input& in)
{
    const ulong keep = in.below(machine.pages);
    const ulong want = in.chance(8) ? in.value32() : in.below(4) + in.chance(192);
    const ulong before = tested_pt().free_pages_count_;
    const ulong got = tested_pt().reserve_huge_pool_keeping(want * HUGE_BYTES, keep);
    INVARIANT(got <= want, "%lu blocks reserved of %lu asked", got, want);
    INVARIANT(got == 0 || tested_pt().free_pages_count_ >= keep, "the pool left %lu pages free, under the %lu kept",
        tested_pt().free_pages_count_, keep);
    INVARIANT(before - tested_pt().free_pages_count_ == got * HUGE_PAGES, "%lu pages left the free list for %lu blocks",
        before - tested_pt().free_pages_count_, got);
    for (ulong block = 0; block < machine.pages / HUGE_PAGES; block++)
    {
        if (!tested_pt().huge_pool_test(tested_pt().huge_pool_in_, block))
            continue;
        for (ulong i = block * HUGE_PAGES; i < (block + 1) * HUGE_PAGES; i++)
        {
            INVARIANT(pages[i].state == PageState::Free, "page %lu reserved for the pool, and not free (state %d)", i,
                pages[i].state);
            pages[i].state = PageState::Pool;
        }
        pool_blocks[block * HUGE_BYTES] = false;
    }
    INVARIANT(pool_blocks.size() == got, "%zu blocks in the bitmap, %lu said reserved", pool_blocks.size(), got);
    /* Highest first: a block still free is below every block the pool
       took, and is left only when the pool had all it asked or the pages
       kept stopped it */
    const bool limited = tested_pt().free_pages_count_ < keep + HUGE_PAGES;
    for (ulong block = 0; block < machine.pages / HUGE_PAGES; block++)
    {
        bool free = true;
        for (ulong i = block * HUGE_PAGES; i < (block + 1) * HUGE_PAGES && free; i++)
            free = pages[i].state == PageState::Free;
        if (!free)
            continue;
        INVARIANT(got == want || limited, "block %lu left free with %lu of %lu reserved and room for more", block, got,
            want);
        INVARIANT(pool_blocks.empty() || block * HUGE_BYTES < pool_blocks.begin()->first,
            "block %lu left free above block 0x%lx of the pool", block, pool_blocks.begin()->first);
    }
    if (got != 0 && got < want)
        fuzz::reached("a pool cut short");
    if (got != 0)
        fuzz::reached("a huge pool");
}

void alloc_huge(fuzz::Input& in)
{
    (void)in;
    ulong pool_free = 0;
    for (const auto& b : pool_blocks)
        pool_free += !b.second;
    const ulong phys = tested_pt().alloc_huge_frame();
    if (phys == 0)
    {
        INVARIANT(pool_free == 0, "alloc_huge_frame failed with %lu blocks free", pool_free);
        return;
    }
    auto it = pool_blocks.find(phys);
    INVARIANT(it != pool_blocks.end() && !it->second, "alloc_huge_frame handed out 0x%lx, no free block of the pool",
        phys);
    it->second = true;
    for (ulong i = 0; i < HUGE_PAGES; i++)
    {
        PageState& st = pages[index(phys) + i];
        INVARIANT(st.state == PageState::Pool, "page %lu of a block handed out is not the pool's (state %d)",
            index(phys) + i, st.state);
        st.state = PageState::Huge;
        check_zeroed(phys + i * PAGE_BYTES, "alloc_huge_frame");
    }
    /* What a guest would write there: the next one handed it must find
       zeros again */
    memset(machine.phys + phys, in.u8() | 1, HUGE_BYTES);
    fuzz::reached("a huge frame");
}

void free_huge(fuzz::Input& in)
{
    std::vector<ulong> held;
    for (const auto& b : pool_blocks)
    {
        if (b.second)
            held.push_back(b.first);
    }
    if (held.empty())
        return;
    const ulong phys = held[in.below(held.size())];
    tested_pt().free_huge_frame(phys);
    pool_blocks[phys] = false;
    for (ulong i = 0; i < HUGE_PAGES; i++)
        pages[index(phys) + i].state = PageState::Pool;
    fuzz::reached("a huge frame back");
}

/* ---- a machine as Setup leaves one ---- */

void boot(fuzz::Input& in)
{
    /* Small, and now and then past 4 MiB: room for a block of the huge
       pool, the first 2MiB being the machine's own */
    machine.pages = in.chance(24) ? 1024 + in.below(1536) : 48 + in.below(in.chance(32) ? 960 : 160);
    void* mem = nullptr;
    INVARIANT(posix_memalign(&mem, PAGE_BYTES, machine.pages * PAGE_BYTES) == 0, "no host memory");
    machine.phys = static_cast<uint8_t*>(mem);
    /* What a page held before: not zeros */
    std::vector<uint8_t> noise = fuzz::noise(in.u32(), PAGE_BYTES);
    for (ulong i = 0; i < machine.pages; i++)
        memcpy(machine.phys + i * PAGE_BYTES, noise.data(), PAGE_BYTES);
    INVARIANT(posix_memalign(&mem, PageTable::HUGE_PAGE_SIZE, WINDOW_SLOTS * PAGE_BYTES) == 0, "no host memory");
    machine.window = static_cast<uint8_t*>(mem);
    ASAN_POISON_MEMORY_REGION(machine.window, WINDOW_SLOTS * PAGE_BYTES);
    INVARIANT(posix_memalign(&mem, PAGE_BYTES, PAGE_BYTES) == 0, "no host memory");
    machine.window_l1 = static_cast<PtePage*>(mem);
    memset(machine.window_l1, 0, PAGE_BYTES);
    for (size_t i = 0; i < WINDOW_SLOTS; i++)
        machine.slot_phys[i] = NO_PHYS;
    machine.page_array = new Page[machine.pages];

    MemoryMap::get_instance().add_region(0, machine.pages * PAGE_BYTES, MemoryMap::USABLE_RAM_TYPE);

    PageTable& pt = tested_pt();
    pt.page_array_ = machine.page_array;
    pt.page_array_count_ = machine.pages;
    pt.highest_phy_addr_ = machine.pages * PAGE_BYTES;
    pt.total_pages_count_ = machine.pages;
    pt.tmp_map_start_ = reinterpret_cast<ulong>(machine.window);
    pt.tmp_map_l1_page_ = machine.window_l1;
    pages.assign(machine.pages, {PageState::Machine, 1, false});
    /* What drain_early_free_list leaves: every usable page on the list, at
       reference count 1 */
    for (ulong i = machine.pages; i-- > RESERVED_PAGES;)
    {
        machine.page_array[i].init(i * PAGE_BYTES);
        pt.free_pages_list_.insert_head(&machine.page_array[i].list_entry);
        pt.free_pages_count_++;
        pages[i].state = PageState::Free;
    }
    for (ulong i = 0; i < RESERVED_PAGES; i++)
        machine.page_array[i].init(i * PAGE_BYTES);

    /* The root, as Setup allocates it */
    Page* root = pt.alloc_page();
    INVARIANT(root != nullptr, "no page for the root");
    pt.root_ = root->get_phy_address();
    pages[index(pt.root_)].state = PageState::Table;

    /* The pool, right after the free list as the boot reserves it */
    if (machine.pages >= 2 * HUGE_PAGES && in.chance(192))
        reserve_pool(in);
}

void run(fuzz::Input& in)
{
    boot(in);
    check("the boot");
    for (int ops = 0; ops < 64 && in.more(); ops++)
    {
        switch (in.u8() % 16)
        {
        case 0:
        case 1:
        case 2:
            alloc(in);
            break;
        case 3:
            alloc_run(in);
            break;
        case 4:
            free_one(in);
            break;
        case 5:
            if (in.chance(48))
                exhaust(in);
            else
                free_one(in);
            break;
        case 6:
        case 7:
        case 8:
            map(in);
            break;
        case 9:
        case 10:
            unmap(in);
            break;
        case 11:
            protect(in);
            break;
        case 12:
        case 13:
            tmp_map(in);
            break;
        case 14:
            if (in.chance(64))
                crowd(in);
            else if (!pool_blocks.empty() && in.chance(192))
            {
                if (in.boolean())
                    alloc_huge(in);
                else
                    free_huge(in);
            }
            else
                frame(in);
            break;
        default:
            map_mmio(in);
            break;
        }
        check("an operation");
        if (in.chance(64))
            check_maps(in, "an operation");
    }
    check_maps(in, "the sequence");
}

void reset()
{
    tmps.clear();
    tmp_slots = 0;
    maps.clear();
    mmio.clear();
    prot.clear();
    pool_blocks.clear();
    pages.clear();
    if (machine.window != nullptr)
    {
        ASAN_UNPOISON_MEMORY_REGION(machine.window, WINDOW_SLOTS * PAGE_BYTES);
        free(machine.window);
        free(machine.window_l1);
        free(machine.phys);
        delete[] machine.page_array;
    }
    machine = Machine();
    PageTable& pt = tested_pt();
    pt.~PageTable();
    new (&pt) PageTable();
    MemoryMap& mmap = MemoryMap::get_instance();
    mmap.~MemoryMap();
    new (&mmap) MemoryMap();
}

}

const fuzz::Target fuzz::THE_TARGET = {"pagetable", reset, run, 1024, 20000};
