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
//
// Free pages are zeroed ahead, as the zeroing tasks do it (zerofree=): a
// page off the other free list onto the zeroed one, and a free block of the
// pool a page at a time -- each page so zeroed reading as zeros, and every
// page on the zeroed list, and every block known zero, still reading so
// while free; every free page on exactly one of the two lists, marked iff
// on the zeroed one, and no page that is not free marked; a guest's
// allocation (alloc_page_keeping, alloc_huge_frame) taking a page or block
// known zero before any other and the kernel's (alloc_page) one that is
// not; and zerofree=verify's reading of what it takes, on for some inputs.
//
// The machine has NUMA nodes now and then (kernel/numa.h): its memory cut
// into ranges of two or three proximity domains -- in no order, far apart, a
// domain with CPUs and no memory, a range given to none, a SLIT that holds
// or does not -- and the free list, built on node 0's as the boot builds it
// before the firmware's tables are read, spread over them
// (spread_free_pages). Every free page is then on its own node's lists, as
// the model works it out from the ranges it gave, and nowhere else; each
// node's counts its lists' lengths, and the machine's their sums; an
// allocation takes from the node of the CPU it runs on (the sequence moves
// itself from CPU to CPU), then from each node in the order of its
// distances; the zeroing takes a node's pages to that node's zeroed list;
// the pool takes each node's blocks from its top; and a block of the pool
// is handed out from the nearest node that has one, known zero first.
#include "host.h"

/* The page table and the memory map are singletons with private
   constructors, which Reset runs again in place; and the page table's
   state after Setup is set up here, member by member. */
#define private public
#include <mm/memory_map.h>
#include <mm/page_table.h>
#include <kernel/numa.h>
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
    /* Free, and on the zeroed list */
    bool zeroed;
};

std::vector<PageState> pages;
/* va -> phys of the sequence's mappings, and of MMIO's pages */
std::map<ulong, ulong> maps;
std::map<ulong, ulong> mmio;
/* What MMIO has been mapped: (phys, pages, write-combining) -> where. The
   same range asked for again is to be that mapping. */
struct MmioKey
{
    ulong phys;
    ulong pages;
    bool write_combining;
    bool operator<(const MmioKey& o) const
    {
        if (phys != o.phys)
            return phys < o.phys;
        if (pages != o.pages)
            return pages < o.pages;
        return write_combining < o.write_combining;
    }
};
std::map<MmioKey, ulong> mmio_known;
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
/* The free ones known zero */
std::set<ulong> pool_zeroed;

const ulong RESERVED_PAGES = 8;
const ulong HUGE_PAGES = PageTable::HUGE_FRAME_PAGES;
const ulong HUGE_BYTES = PageTable::HUGE_FRAME_SIZE;

PageTable& tested_pt()
{
    return PageTable::get_instance();
}

/* The NUMA topology boot gave the machine, as the model has it: what it
   told Numa, and what follows from that worked out here, not asked of Numa
   -- each page's node (its range's domain's place among the domains,
   ascending; 0 for a page no range covers), and each node's fallback order
   (itself, then by distance and index, the SLIT's distances where they hold
   together, 10 and 20 where not). */
struct Topology
{
    ulong nodes;
    std::vector<ulong> page_node;
    std::vector<bool> covered;
    /* The CPUs boot gave Numa a node for; any other is node 0's */
    std::map<ulong, ulong> cpu_node;
    ulong cpus;
    std::vector<std::vector<ulong>> fallback;
};
Topology topo;

/* The node of the CPU the sequence runs on */
ulong current_node()
{
    if (topo.nodes == 1)
        return 0;
    auto it = topo.cpu_node.find(fuzz::current_cpu);
    return it == topo.cpu_node.end() ? 0 : it->second;
}

/* A block of the pool is the node's its first page is */
ulong block_node(ulong block)
{
    return topo.page_node[block * HUGE_PAGES];
}

bool block_covered(ulong block)
{
    return topo.covered[block * HUGE_PAGES];
}

ulong place_of(const std::vector<u32>& sorted, u32 domain)
{
    return static_cast<ulong>(std::lower_bound(sorted.begin(), sorted.end(), domain) - sorted.begin());
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

/* A free list walked: each page a page, once on the two lists, marked iff
   the list is the zeroed one. Its length. */
ulong walk_free_list(stdlib::ListEntry* head, bool zeroed_list, ulong node, std::set<ulong>& free,
    std::set<ulong>& zeroed, const char* after)
{
    ulong on_list = 0;
    for (stdlib::ListEntry* e = head->flink; e != head; e = e->flink)
    {
        Page* p = CONTAINING_RECORD(e, Page, list_entry);
        ulong i = static_cast<ulong>(p - machine.page_array);
        INVARIANT(i < machine.pages, "a free list holds a page that is no page, after %s", after);
        INVARIANT(topo.page_node[i] == node, "page %lu, node %lu's, on node %lu's %s list, after %s", i,
            topo.page_node[i], node, zeroed_list ? "zeroed" : "other", after);
        INVARIANT(free.insert(i).second, "page %lu twice on the free lists, after %s", i, after);
        INVARIANT(p->is_zeroed() == zeroed_list, "page %lu on the %s list %s the mark, after %s", i,
            zeroed_list ? "zeroed" : "other", zeroed_list ? "without" : "with", after);
        if (zeroed_list)
            zeroed.insert(i);
        on_list++;
        INVARIANT(on_list <= machine.pages, "a free list loops, after %s", after);
    }
    return on_list;
}

/* Pages from phys on known to be zeros, read every 509 bytes: nothing has
   written to a free page since it was zeroed. Physical memory as it is: the
   caller has written the window back. */
void check_sampled_zero(ulong phys, ulong count, const char* what, const char* after)
{
    for (ulong b = 0; b < count * PAGE_BYTES; b += 509)
        INVARIANT(machine.phys[phys + b] == 0, "%s at 0x%lx is not zeros at +%lu, after %s", what, phys, b, after);
}

/* Everything the model says, against the machine. */
void check(const char* after)
{
    write_back_all();

    /* Every page on a free list is a free one, once, on one of the two,
       marked iff on the zeroed one; the counts are the lists' lengths */
    std::set<ulong> free;
    std::set<ulong> zeroed;
    ulong on_other = 0;
    ulong on_zeroed = 0;
    for (ulong n = 0; n < PageTable::MAX_NODES; n++)
    {
        PageTable::NodeFree& lists = tested_pt().node_free_[n];
        const ulong other_n = walk_free_list(&lists.free_list, false, n, free, zeroed, after);
        const ulong zeroed_n = walk_free_list(&lists.zeroed_list, true, n, free, zeroed, after);
        INVARIANT(lists.free_count == other_n + zeroed_n, "node %lu's free_count %lu with %lu on its lists, after %s",
            n, lists.free_count, other_n + zeroed_n, after);
        INVARIANT(lists.zeroed_count == zeroed_n, "node %lu's zeroed_count %lu with %lu on its zeroed list, after %s",
            n, lists.zeroed_count, zeroed_n, after);
        on_other += other_n;
        on_zeroed += zeroed_n;
    }
    INVARIANT(tested_pt().free_pages_count_ == on_other + on_zeroed,
        "free_pages_count_ %lu with %lu on the lists, after %s", tested_pt().free_pages_count_, on_other + on_zeroed,
        after);
    INVARIANT(tested_pt().zeroed_pages_count_ == on_zeroed, "zeroed_pages_count_ %lu with %lu on the zeroed list, after %s",
        tested_pt().zeroed_pages_count_, on_zeroed, after);

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
        const bool known_zero = pool_zeroed.count(b.first) != 0;
        INVARIANT(!known_zero || !b.second, "block 0x%lx held and known zero, after %s", b.first, after);
        INVARIANT(tested_pt().huge_pool_test(tested_pt().huge_pool_zeroed_bits_, block) == known_zero,
            "block 0x%lx %s by the bitmap, after %s", b.first, known_zero ? "not known zero" : "known zero", after);
        if (known_zero)
            check_sampled_zero(b.first, HUGE_PAGES, "a block known zero", after);
    }
    INVARIANT(tested_pt().get_huge_pool_blocks() == pool_blocks.size() && tested_pt().get_huge_pool_free() == pool_free,
        "the pool says %lu blocks, %lu free, where it has %zu, %lu free, after %s", tested_pt().get_huge_pool_blocks(),
        tested_pt().get_huge_pool_free(), pool_blocks.size(), pool_free, after);
    INVARIANT(tested_pt().get_huge_pool_zeroed() == pool_zeroed.size(), "the pool says %lu blocks known zero, where it has %zu, after %s",
        tested_pt().get_huge_pool_zeroed(), pool_zeroed.size(), after);
    /* The block the zeroing is part way through: free, not known zero, and
       the pages done of it zeros */
    if (tested_pt().huge_zero_block_ != PageTable::NO_HUGE_BLOCK)
    {
        const ulong at = tested_pt().huge_zero_block_ * HUGE_BYTES;
        auto it = pool_blocks.find(at);
        INVARIANT(it != pool_blocks.end() && !it->second && !pool_zeroed.count(at),
            "the zeroing is part way through block 0x%lx, no free block of the pool not known zero, after %s", at, after);
        INVARIANT(tested_pt().huge_zero_next_ > 0 && tested_pt().huge_zero_next_ < HUGE_PAGES,
            "the zeroing at page %lu of block 0x%lx, after %s", tested_pt().huge_zero_next_, at, after);
        check_sampled_zero(at, tested_pt().huge_zero_next_, "the block being zeroed", after);
    }

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
            s.zeroed = false;
            fuzz::reached("a table allocated on the way down");
        }
        INVARIANT(s.state == PageState::Table, "page 0x%lx is a table, and %s's too (state %d), after %s", phys,
            "the sequence", s.state, after);
    }

    for (ulong i = 0; i < machine.pages; i++)
    {
        PageState& s = pages[i];
        Page& p = machine.page_array[i];
        INVARIANT(p.get_phy_address() == i * PAGE_BYTES, "page %lu's descriptor says 0x%lx", i, p.get_phy_address());
        if (s.state == PageState::Free)
        {
            INVARIANT(free.count(i), "page %lu is free and not on the free list, after %s", i, after);
            /* A zeroed page the kernel took for a table on the way down a walk
               and gave back -- a map that failed, a window cut short -- is on
               the other list now, unmarked, as it should be: the model did not
               see it go. The other way round, only zero_free_page puts a page
               on the zeroed list, and the model sees each. */
            if (s.zeroed && !zeroed.count(i))
            {
                s.zeroed = false;
                fuzz::reached("a zeroed page through a table and back");
            }
            INVARIANT(zeroed.count(i) == (s.zeroed ? 1u : 0u), "page %lu is on the zeroed list, and the model never saw "
                "it zeroed, after %s", i, after);
            if (s.zeroed)
                check_sampled_zero(i * PAGE_BYTES, 1, "a page on the zeroed list", after);
        }
        else
        {
            INVARIANT(!free.count(i), "page %lu (state %d) is on the free list, after %s", i, s.state, after);
            INVARIANT(!p.is_zeroed(), "page %lu (state %d) carries the zeroed mark, after %s", i, s.state, after);
        }
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

/* The page an allocation meant for `node` should take: of each node in its
   fallback order, the head of the list it prefers, else of the other;
   nullptr when every one is empty */
Page* expected_page_on(bool zeroed_first, ulong node)
{
    for (ulong n : topo.fallback[node])
    {
        PageTable::NodeFree& lists = tested_pt().node_free_[n];
        stdlib::ListEntry* first = zeroed_first ? &lists.zeroed_list : &lists.free_list;
        stdlib::ListEntry* second = zeroed_first ? &lists.free_list : &lists.zeroed_list;
        stdlib::ListEntry* list = !first->is_empty() ? first : second;
        if (!list->is_empty())
            return CONTAINING_RECORD(list->flink, Page, list_entry);
    }
    return nullptr;
}

Page* expected_page(bool zeroed_first)
{
    return expected_page_on(zeroed_first, current_node());
}

/* A page an allocation took, as the model has it: held, and whether it was
   one known zero */
void took(ulong phys, const char* by)
{
    PageState& s = pages[index(phys)];
    INVARIANT(s.state == PageState::Free, "%s handed out page 0x%lx, which is not free (state %d)", by, phys, s.state);
    s.state = PageState::Held;
    s.zeroed = false;
    check_zeroed(phys, by);
}

void alloc(fuzz::Input& in)
{
    (void)in;
    Page* want = expected_page(false);
    const bool want_zeroed = want != nullptr && pages[static_cast<ulong>(want - machine.page_array)].zeroed;
    Page* p = tested_pt().alloc_page();
    if (p == nullptr)
    {
        INVARIANT(tested_pt().free_pages_count_ == 0, "alloc_page failed with %lu pages free", tested_pt().free_pages_count_);
        fuzz::reached("memory exhausted");
        return;
    }
    INVARIANT(p == want, "alloc_page took page 0x%lx, not the head of the list it prefers", p->get_phy_address());
    took(p->get_phy_address(), "alloc_page");
    fuzz::reached(want_zeroed ? "alloc_page took a zeroed page" : "alloc_page took one to zero");
}

/* A guest's allocation for a node (its memory placed there): a page known
   zero first, of its lists, then the others' in its fallback order; none once
   `keep` or fewer are free */
void alloc_on(fuzz::Input& in)
{
    const ulong node = in.below(topo.nodes);
    const ulong free_now = tested_pt().free_pages_count_;
    const ulong keep = in.chance(8) ? free_now : in.below(free_now + 1);
    Page* want = expected_page_on(true, node);
    Page* p = tested_pt().alloc_page_keeping_on(node, keep);
    if (free_now <= keep)
    {
        INVARIANT(p == nullptr, "alloc_page_keeping_on(%lu, %lu) took a page with %lu free", node, keep, free_now);
        return;
    }
    INVARIANT(p != nullptr && p == want, "alloc_page_keeping_on(%lu, %lu) with %lu free took %s, not the head of the "
        "list it prefers", node, keep, free_now, p == nullptr ? "nothing" : "another page");
    took(p->get_phy_address(), "alloc_page_keeping_on");
    const ulong i = static_cast<ulong>(p - machine.page_array);
    fuzz::reached(topo.page_node[i] == node ? "a guest's page of its own node" : "a guest's page of another node");
}

/* The sequence moved to another CPU -- now and then one boot named to no
   node, which is node 0's */
void move_cpu(fuzz::Input& in)
{
    fuzz::current_cpu = in.below(topo.cpus + 1);
    fuzz::reached("moved to another CPU");
}

/* A guest's allocation: a page known zero first, and none once `keep` or
   fewer are free */
void alloc_keeping(fuzz::Input& in)
{
    const ulong free_now = tested_pt().free_pages_count_;
    const ulong keep = in.chance(8) ? free_now : in.below(free_now + 1);
    Page* want = expected_page(true);
    const bool want_zeroed = want != nullptr && pages[static_cast<ulong>(want - machine.page_array)].zeroed;
    Page* p = tested_pt().alloc_page_keeping(keep);
    if (free_now <= keep)
    {
        INVARIANT(p == nullptr, "alloc_page_keeping(%lu) took a page with %lu free", keep, free_now);
        fuzz::reached("alloc_page_keeping kept its pages");
        return;
    }
    INVARIANT(p != nullptr && p == want, "alloc_page_keeping(%lu) with %lu free took %s, not the head of the list it "
        "prefers", keep, free_now, p == nullptr ? "nothing" : "another page");
    took(p->get_phy_address(), "alloc_page_keeping");
    fuzz::reached(want_zeroed ? "alloc_page_keeping took a zeroed page" : "alloc_page_keeping took one to zero");
}

/* The zeroing tasks' work, a step or a run of them: a page off the other
   list's cold end onto the zeroed list, while more than `keep` are free;
   and a page of a free block of the pool not known zero, a block known zero
   once all its pages are */
void zero_step(fuzz::Input& in)
{
    /* The pool's zeroing now and then with no pool: nothing to do */
    const bool huge = pool_blocks.empty() ? in.chance(16) : in.boolean();
    /* Now and then a block's worth and more, for the pool's oftener: what
       makes a block known zero, and an allocation that takes one */
    const ulong steps = in.chance(huge ? 96 : 8) ? HUGE_PAGES + in.below(64) : 1 + in.below(16);
    const ulong free_now = tested_pt().free_pages_count_;
    const ulong keep = in.chance(8) ? free_now : in.below(free_now + 1);
    for (ulong n = 0; n < steps; n++)
    {
        if (!huge)
        {
            const ulong node = in.below(topo.nodes);
            PageTable::NodeFree& lists = tested_pt().node_free_[node];
            stdlib::ListEntry* other = &lists.free_list;
            Page* tail = other->is_empty() ? nullptr : CONTAINING_RECORD(other->blink, Page, list_entry);
            const bool want = tested_pt().free_pages_count_ > keep && tail != nullptr;
            const bool did = tested_pt().zero_free_page(node, keep);
            INVARIANT(did == want, "zero_free_page(%lu, %lu) with %lu free %s", node, keep,
                tested_pt().free_pages_count_, did ? "zeroed a page" : "zeroed none");
            if (!did)
                continue;
            Page* head = CONTAINING_RECORD(lists.zeroed_list.flink, Page, list_entry);
            INVARIANT(head == tail, "zero_free_page put another page than the other list's tail on the zeroed list");
            const ulong i = static_cast<ulong>(head - machine.page_array);
            INVARIANT(pages[i].state == PageState::Free && !pages[i].zeroed, "zero_free_page zeroed page %lu (state %d, "
                "%s)", i, pages[i].state, pages[i].zeroed ? "zeroed" : "not");
            pages[i].zeroed = true;
            check_zeroed(i * PAGE_BYTES, "zero_free_page");
            fuzz::reached("a free page zeroed");
            continue;
        }

        const ulong block_before = tested_pt().huge_zero_block_;
        const ulong next_before = tested_pt().huge_zero_next_;
        bool any = false;
        for (const auto& b : pool_blocks)
            any = any || (!b.second && !pool_zeroed.count(b.first));
        const bool did = tested_pt().zero_free_huge_page();
        INVARIANT(did == any, "zero_free_huge_page %s with %s", did ? "zeroed a page" : "zeroed none",
            any ? "a free block not known zero" : "none");
        if (!did)
            break;
        ulong block = block_before;
        ulong page = next_before;
        if (block_before == PageTable::NO_HUGE_BLOCK)
        {
            block = tested_pt().huge_zero_block_;
            page = 0;
            INVARIANT(block != PageTable::NO_HUGE_BLOCK && tested_pt().huge_zero_next_ == 1,
                "zero_free_huge_page started a block and is not at its second page");
            auto it = pool_blocks.find(block * HUGE_BYTES);
            INVARIANT(it != pool_blocks.end() && !it->second && !pool_zeroed.count(block * HUGE_BYTES),
                "zero_free_huge_page started on block %lu, no free block not known zero", block);
        }
        check_zeroed(block * HUGE_BYTES + page * PAGE_BYTES, "zero_free_huge_page");
        if (page + 1 == HUGE_PAGES)
        {
            INVARIANT(tested_pt().huge_zero_block_ == PageTable::NO_HUGE_BLOCK, "a block's last page zeroed, and the "
                "zeroing still on it");
            pool_zeroed.insert(block * HUGE_BYTES);
            check_sampled_zero(block * HUGE_BYTES, HUGE_PAGES, "a block just zeroed", "zero_free_huge_page");
            fuzz::reached("a block of the pool zeroed");
        }
        else
        {
            INVARIANT(tested_pt().huge_zero_block_ == block && tested_pt().huge_zero_next_ == page + 1,
                "the zeroing went from block %lu page %lu to block %lu page %lu", block, page,
                tested_pt().huge_zero_block_, tested_pt().huge_zero_next_);
        }
    }
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
        if (pages[index(phys)].zeroed)
            fuzz::reached("a run with a zeroed page in it");
        took(phys, "alloc_contiguous_pages");
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
        pages[index(p->get_phy_address())].zeroed = false;
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
    s.zeroed = false;
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
    /* The pages come back in the order unmapped, for the caller to free
       once its shootdown is done; here there is one CPU */
    std::vector<Page*> frames(count, nullptr);
    tested_pt().unmap_pages(va, count, free ? frames.data() : nullptr);
    if (free)
    {
        for (ulong i = 0; i < count; i++)
            INVARIANT(frames[i] == page_of(maps[va + i * PAGE_BYTES]), "unmap_pages(0x%lx) hands back another "
                "page for 0x%lx", va, va + i * PAGE_BYTES);
        tested_pt().free_pages(frames.data(), count);
    }
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
    /* A device past the machine's memory, of one to three pages, mapped
       whole or from its second page on */
    const ulong pa = (machine.pages + in.below(64)) * PAGE_BYTES;
    const ulong pages = 1 + in.below(3);
    hal::write_combining = in.boolean();
    const bool ask_wc = in.boolean();
    const bool wc = ask_wc && hal::write_combining;
    const ulong size = (pages - 1) * PAGE_BYTES + 1 + in.below(PAGE_BYTES);
    ulong got = tested_pt().map_mmio_region(pa, size, ask_wc ? PageTable::MmioWriteCombining
                                                             : PageTable::MmioUncached);
    if (got == 0)
    {
        fuzz::reached("an MMIO map that failed on the way");
        return;
    }
    INVARIANT(got >= PageTable::MMIO_WINDOW && got < PageTable::MMIO_WINDOW + PageTable::TOP_ENTRY_SPAN &&
        got % PAGE_BYTES == 0, "MMIO at 0x%lx mapped at 0x%lx, outside its window", pa, got);

    /* The same range again is the mapping it has; so is a range inside one
       mapped before, as the same pages of it */
    ulong expected = 0;
    for (const auto& k : mmio_known)
    {
        if (k.first.write_combining == wc && pa >= k.first.phys &&
            pa + pages * PAGE_BYTES <= k.first.phys + k.first.pages * PAGE_BYTES)
        {
            expected = k.second + (pa - k.first.phys);
            break;
        }
    }
    if (expected != 0 && mmio_known.size() <= PageTable::MMIO_MAPPINGS_MAX)
    {
        /* Any mapping that holds the range will do: the kernel takes the
           first it noted */
        bool held = false;
        for (const auto& k : mmio_known)
        {
            if (k.first.write_combining == wc && pa >= k.first.phys &&
                pa + pages * PAGE_BYTES <= k.first.phys + k.first.pages * PAGE_BYTES &&
                got == k.second + (pa - k.first.phys))
                held = true;
        }
        INVARIANT(held, "MMIO at 0x%lx, mapped before, mapped again at 0x%lx", pa, got);
        fuzz::reached("an MMIO map of a range mapped already");
    }
    else
    {
        for (ulong i = 0; i < pages; i++)
        {
            const ulong va = got + i * PAGE_BYTES;
            INVARIANT(!mmio.count(va) && !maps.count(va), "MMIO at 0x%lx given 0x%lx, which is taken", pa, va);
            mmio[va] = pa + i * PAGE_BYTES;
        }
        /* The page after it is left unmapped */
        INVARIANT(!leaf_of(got + pages * PAGE_BYTES).present(), "MMIO at 0x%lx with no unmapped page after it", pa);
        /* A mapping the table notes -- it notes MMIO_MAPPINGS_MAX of them --
           says how far it runs from any address in it, and nothing of the
           page left unmapped after it: what a user of somebody else's
           mapping holds an offset into it to (MsixTable::setup) */
        if (mmio_known.size() < PageTable::MMIO_MAPPINGS_MAX)
        {
            const ulong inside = in.below(pages * PAGE_BYTES);
            INVARIANT(tested_pt().mmio_mapped_span(got + inside) == pages * PAGE_BYTES - inside,
                "mmio_mapped_span(0x%lx) = 0x%lx for a mapping of %lu pages at 0x%lx", got + inside,
                tested_pt().mmio_mapped_span(got + inside), pages, got);
            INVARIANT(tested_pt().mmio_mapped_span(got + pages * PAGE_BYTES) == 0,
                "mmio_mapped_span says the page after a mapping is mapped");
            fuzz::reached("a noted MMIO mapping's span");
        }
        mmio_known[MmioKey{pa, pages, wc}] = got;
    }
    for (ulong i = 0; i < pages; i++)
    {
        Pte leaf = leaf_of(got + i * PAGE_BYTES);
        INVARIANT(leaf.present() && writable(leaf) && !executable(leaf),
            "an MMIO mapping that is not there, cannot be written or can be executed");
    }
    fuzz::reached("an MMIO map");
}

/* The huge pool, reserved as the boot reserves it: whole free blocks, the
   highest first, leaving keep pages free. */
void reserve_pool(fuzz::Input& in)
{
    const ulong keep = in.below(machine.pages);
    const ulong want = in.chance(8) ? in.value32() : in.below(4) + in.chance(192);
    const ulong before = tested_pt().free_pages_count_;
    /* Each node's share of what is kept free: as much as it has of the free
       pages, worked out as the pool's documentation has it */
    std::vector<ulong> node_keep(topo.nodes, 0);
    for (ulong n = 0; n < topo.nodes && before != 0; n++)
        node_keep[n] = (topo.nodes == 1) ? keep : keep * tested_pt().node_free_[n].free_count / before;
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
            pages[i].zeroed = false;
        }
        pool_blocks[block * HUGE_BYTES] = false;
    }
    INVARIANT(pool_blocks.size() == got, "%zu blocks in the bitmap, %lu said reserved", pool_blocks.size(), got);
    /* Highest first, of each node's blocks: a block still free is below
       every block the pool took of its node, and is left only when the pool
       had all it asked, or the pages kept -- the machine's, or its node's
       share of them -- stopped it */
    const bool limited = tested_pt().free_pages_count_ < keep + HUGE_PAGES;
    for (ulong block = 0; block < machine.pages / HUGE_PAGES; block++)
    {
        bool free = true;
        for (ulong i = block * HUGE_PAGES; i < (block + 1) * HUGE_PAGES && free; i++)
            free = pages[i].state == PageState::Free;
        if (!free)
            continue;
        const ulong node = block_node(block);
        const ulong node_free = tested_pt().node_free_[node].free_count;
        const bool node_limited = node_free < node_keep[node] + HUGE_PAGES;
        INVARIANT(got == want || limited || node_limited, "block %lu, node %lu's, left free with %lu of %lu reserved "
            "and room for more", block, node, got, want);
        for (const auto& b : pool_blocks)
        {
            if (block_node(b.first / HUGE_BYTES) != node)
                continue;
            INVARIANT(block * HUGE_BYTES < b.first, "block %lu left free above block 0x%lx of the pool, both node %lu's",
                block, b.first, node);
        }
    }
    if (got != 0 && got < want)
        fuzz::reached("a pool cut short");
    if (got != 0)
        fuzz::reached("a huge pool");
}

/* The L2 entry that maps va, read the way the MMU reads it; Value 0 if a
   level above it is missing, or is a block. */
Pte l2_of(ulong va)
{
    write_back_all();
    Pte none;
    none.value = 0;
    ulong phys = tested_pt().root_;
    const ulong idx[] = {Pte::l4_index(va), Pte::l3_index(va), Pte::l2_index(va)};
    for (int level = 0; level < 3; level++)
    {
        const PtePage* t = reinterpret_cast<const PtePage*>(machine.phys + phys);
        Pte e = t->entry[idx[level]];
        if (level == 2)
            return e;
        if (!e.present() || e.huge())
            return none;
        phys = e.address();
    }
    return none;
}

/* The pool's window, as the boot makes it right after the pool -- or later,
   once, with the free list or the tmp_map window as the sequence has left
   them: every block of the pool a 2MiB leaf at HUGE_POOL_WINDOW plus its
   address, writable and never executable, and every page of a block found
   there -- or, where it says it was not made, nothing of it at all: no
   table left, no page of the free list gone, no page found. */
void pool_window(fuzz::Input& in)
{
    (void)in;
    if (tested_pt().huge_pool_window_on_)
        return;
    const ulong before = tested_pt().free_pages_count_;
    std::set<ulong> tables_before;
    collect_tables(tables_before);
    const bool made = tested_pt().map_huge_pool_window();
    std::set<ulong> tables_after;
    collect_tables(tables_after);
    const ulong taken = before - tested_pt().free_pages_count_;
    if (!made)
    {
        INVARIANT(taken == 0, "a window not made took %lu pages off the free list", taken);
        INVARIANT(tables_after == tables_before, "a window not made left %zu tables where there were %zu",
            tables_after.size(), tables_before.size());
        write_back_all();
        const PtePage* top = reinterpret_cast<const PtePage*>(machine.phys + tested_pt().root_);
        Pte entry = top->entry[Pte::l4_index(PageTable::HUGE_POOL_WINDOW)];
        INVARIANT(!entry.present(), "a window not made left its top-level entry");
        for (const auto& b : pool_blocks)
            INVARIANT(tested_pt().huge_pool_window(b.first) == 0, "block 0x%lx found in a window not made", b.first);
        fuzz::reached(pool_blocks.empty() ? "a window of no pool" : "a pool window not made");
        return;
    }
    INVARIANT(!pool_blocks.empty(), "a window made of no pool");
    INVARIANT(taken == tables_after.size() - tables_before.size(), "the window took %lu pages for %zu tables", taken,
        tables_after.size() - tables_before.size());
    for (const auto& b : pool_blocks)
    {
        const ulong va = PageTable::HUGE_POOL_WINDOW + b.first;
        Pte e = l2_of(va);
        INVARIANT(e.present() && e.huge() && e.address() == b.first, "block 0x%lx not at 0x%lx in the window",
            b.first, va);
        INVARIANT(writable(e) && !executable(e), "block 0x%lx in the window %s", b.first,
            writable(e) ? "executable" : "read-only");
        const ulong page = b.first + in.below(HUGE_PAGES) * PAGE_BYTES;
        INVARIANT(tested_pt().huge_pool_window(page) == PageTable::HUGE_POOL_WINDOW + page,
            "page 0x%lx of the pool found at 0x%lx", page, tested_pt().huge_pool_window(page));
    }
    for (ulong i = 0; i < machine.pages; i += 1 + in.below(64))
    {
        if (pages[i].state != PageState::Pool && pages[i].state != PageState::Huge)
            INVARIANT(tested_pt().huge_pool_window(i * PAGE_BYTES) == 0, "page %lu, no pool's, found in the window",
                i);
    }
    fuzz::reached("a pool window");
}

/* What a block allocation meant for `node` takes, on a machine of nodes: of
   each node in its fallback order, a block of that node's ranges known zero,
   else the block the zeroing is part way through where it is that node's,
   else any free block of that node's ranges; past every node, a block no
   range has the first page of, known zero first. True when `phys` is what it
   should be. */
bool huge_as_expected(ulong node, ulong zeroing, ulong phys)
{
    const ulong got = phys / HUGE_BYTES;
    for (ulong n : topo.fallback[node])
    {
        bool zeroed_here = false;
        bool free_here = false;
        for (const auto& b : pool_blocks)
        {
            const ulong block = b.first / HUGE_BYTES;
            if (b.second || !block_covered(block) || block_node(block) != n)
                continue;
            free_here = true;
            zeroed_here = zeroed_here || pool_zeroed.count(b.first) != 0;
        }
        if (zeroed_here)
            return pool_zeroed.count(phys) != 0 && block_covered(got) && block_node(got) == n;
        if (zeroing != PageTable::NO_HUGE_BLOCK && block_node(zeroing) == n)
            return got == zeroing;
        if (free_here)
            return pool_zeroed.count(phys) == 0 && block_covered(got) && block_node(got) == n;
    }
    if (!pool_zeroed.empty())
        return pool_zeroed.count(phys) != 0 && !block_covered(got);
    return !block_covered(got);
}

void alloc_huge(fuzz::Input& in)
{
    ulong pool_free = 0;
    for (const auto& b : pool_blocks)
        pool_free += !b.second;
    const bool on = topo.nodes > 1 && in.chance(85);
    const ulong node = on ? in.below(topo.nodes) : current_node();
    const ulong zeroing = tested_pt().huge_zero_block_;
    const ulong phys = on ? tested_pt().alloc_huge_frame_on(node) : tested_pt().alloc_huge_frame();
    if (phys == 0)
    {
        INVARIANT(pool_free == 0, "alloc_huge_frame failed with %lu blocks free", pool_free);
        return;
    }
    auto it = pool_blocks.find(phys);
    INVARIANT(it != pool_blocks.end() && !it->second, "alloc_huge_frame handed out 0x%lx, no free block of the pool",
        phys);
    if (topo.nodes == 1)
    {
        /* A block known zero first; with none, the block the zeroing is part
           way through, which the zeroing then leaves for another */
        INVARIANT(pool_zeroed.empty() || pool_zeroed.count(phys), "alloc_huge_frame handed out 0x%lx with %zu blocks "
            "known zero, and it none of them", phys, pool_zeroed.size());
        INVARIANT(!pool_zeroed.empty() || zeroing == PageTable::NO_HUGE_BLOCK || zeroing == phys / HUGE_BYTES,
            "alloc_huge_frame handed out 0x%lx with no block known zero, not block %lu the zeroing is part way through",
            phys, zeroing);
    }
    else
    {
        INVARIANT(huge_as_expected(node, zeroing, phys), "alloc_huge_frame%s for node %lu handed out 0x%lx, node %lu's%s%s, "
            "not the nearest node's first choice", on ? "_on" : "", node, phys, block_node(phys / HUGE_BYTES),
            block_covered(phys / HUGE_BYTES) ? "" : " by no range", pool_zeroed.count(phys) ? ", known zero" : "");
        fuzz::reached(block_node(phys / HUGE_BYTES) == node ? "a huge frame of its own node" :
            "a huge frame of another node");
    }
    fuzz::reached(pool_zeroed.count(phys) ? "a huge frame known zero" : "a huge frame to zero");
    pool_zeroed.erase(phys);
    if (zeroing == phys / HUGE_BYTES)
    {
        INVARIANT(tested_pt().huge_zero_block_ == PageTable::NO_HUGE_BLOCK, "block 0x%lx handed out part way through "
            "its zeroing, and the zeroing still on it", phys);
        fuzz::reached("a huge frame taken from under the zeroing, its rest zeroed");
    }
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

/* The machine's NUMA topology, told to Numa as the SRAT and the SLIT tell it,
   and sealed: one node mostly -- no table, or one domain -- else two or
   three domains, in no order and now and then far apart, the memory cut into
   ranges of them, every domain with memory one at least, and now and then a
   domain of CPUs alone or a range given to none; CPUs of each, and some boot
   names to none; a SLIT that holds, one that does not, or none. What Numa
   says of every page, CPU and fallback is held to what the model works out
   from that alone. */
void make_topology(fuzz::Input& in)
{
    Numa& numa = Numa::get_instance();
    topo.page_node.assign(machine.pages, 0);
    topo.covered.assign(machine.pages, false);
    topo.cpu_node.clear();
    topo.cpus = 1 + in.below(6);

    const ulong want = in.chance(128) ? 1 : 2 + in.below(2);
    if (want == 1 && in.boolean())
    {
        numa.seal((1UL << topo.cpus) - 1);
        topo.nodes = 1;
        topo.fallback.assign(1, std::vector<ulong>(1, 0));
        INVARIANT(numa.get_node_count() == 1, "no SRAT, and %lu nodes", numa.get_node_count());
        return;
    }

    std::vector<u32> domains;
    while (domains.size() < want)
    {
        u32 d = in.chance(32) ? in.value32() : static_cast<u32>(in.below(8));
        /* Taken already -- or the input run out, and every value the same:
           the next one free */
        while (std::find(domains.begin(), domains.end(), d) != domains.end())
            d++;
        domains.push_back(d);
    }
    std::vector<u32> sorted = domains;
    std::sort(sorted.begin(), sorted.end());
    /* Every domain a CPU at least, so that every one is a node, the one of
       CPUs alone too */
    topo.cpus = std::max<ulong>(topo.cpus, want);
    const ulong cpu_mask = (1UL << topo.cpus) - 1;

    const ulong with_memory = (want > 1 && in.chance(16)) ? want - 1 : want;
    if (with_memory < want)
        fuzz::reached("a node of CPUs alone");

    /* The cuts: a page boundary, or a block's -- the pool's blocks whole in
       a node, or across two */
    const ulong segments = with_memory + in.below(3);
    const bool aligned = machine.pages >= 4 * HUGE_PAGES && in.boolean();
    std::vector<ulong> cuts;
    for (int tries = 0; tries < 64 && cuts.size() + 1 < segments; tries++)
    {
        ulong c = 1 + in.below(machine.pages - 1);
        if (aligned)
            c = (c / HUGE_PAGES) * HUGE_PAGES;
        if (c != 0 && c < machine.pages && std::find(cuts.begin(), cuts.end(), c) == cuts.end())
            cuts.push_back(c);
    }
    std::sort(cuts.begin(), cuts.end());
    std::vector<ulong> bounds(1, 0);
    bounds.insert(bounds.end(), cuts.begin(), cuts.end());
    bounds.push_back(machine.pages);

    /* The first ranges one each of the domains with memory, in no order;
       the rest any of theirs, or none's */
    std::vector<u32> order(domains.begin(), domains.begin() + with_memory);
    for (ulong i = order.size(); i > 1; i--)
        std::swap(order[i - 1], order[in.below(i)]);
    for (ulong seg = 0; seg + 1 < bounds.size(); seg++)
    {
        u32 d = 0;
        if (seg < order.size())
            d = order[seg];
        else if (in.chance(40))
        {
            fuzz::reached("a range of no node's");
            continue;
        }
        else
            d = domains[in.below(with_memory)];
        const ulong b = bounds[seg];
        const ulong e = bounds[seg + 1];
        INVARIANT(numa.add_memory(d, b * PAGE_BYTES, (e - b) * PAGE_BYTES), "range %lu-%lu of domain %u refused", b, e, d);
        for (ulong i = b; i < e; i++)
        {
            topo.page_node[i] = place_of(sorted, d);
            topo.covered[i] = true;
        }
    }

    for (ulong c = 0; c < topo.cpus; c++)
    {
        if (c >= want && in.chance(24))
            continue;
        const u32 d = (c < want) ? domains[c] : domains[in.below(want)];
        INVARIANT(numa.add_cpu(d, c), "CPU %lu of domain %u refused", c, d);
        topo.cpu_node[c] = place_of(sorted, d);
    }

    /* The distances: the SLIT's where it holds -- a diagonal of 10, nothing
       under it -- 10 and 20 where it does not, or there is none */
    std::vector<std::vector<ulong>> dist(want, std::vector<ulong>(want, 0));
    const ulong slit = in.below(3);
    if (slit != 0)
    {
        for (ulong i = 0; i < want; i++)
        {
            for (ulong j = 0; j < want; j++)
                dist[i][j] = (i == j) ? Numa::LOCAL_DISTANCE : Numa::LOCAL_DISTANCE + in.below(40);
        }
        if (slit == 2)
        {
            const ulong i = in.below(want);
            const ulong j = in.below(want);
            dist[i][j] = (i == j) ? Numa::LOCAL_DISTANCE + 1 + in.below(8) : in.below(Numa::LOCAL_DISTANCE);
            fuzz::reached("a SLIT that does not hold");
        }
        for (ulong i = 0; i < want; i++)
        {
            for (ulong j = 0; j < want; j++)
                numa.set_distance(i, j, static_cast<u8>(dist[i][j]));
        }
    }
    if (slit != 1)
    {
        for (ulong i = 0; i < want; i++)
        {
            for (ulong j = 0; j < want; j++)
                dist[i][j] = (i == j) ? Numa::LOCAL_DISTANCE : Numa::REMOTE_DISTANCE;
        }
    }

    numa.seal(cpu_mask);
    topo.nodes = want;
    INVARIANT(numa.get_node_count() == want, "%lu domains, and %lu nodes", want, numa.get_node_count());
    topo.fallback.assign(want, std::vector<ulong>());
    for (ulong n = 0; n < want; n++)
    {
        std::vector<ulong>& f = topo.fallback[n];
        for (ulong m = 0; m < want; m++)
        {
            if (m != n)
                f.push_back(m);
        }
        std::stable_sort(f.begin(), f.end(), [&](ulong a, ulong b) { return dist[n][a] < dist[n][b]; });
        f.insert(f.begin(), n);
        for (ulong r = 0; r < want; r++)
            INVARIANT(numa.get_fallback(n, r) == f[r], "node %lu's fallback %lu is %lu, not %lu", n, r,
                numa.get_fallback(n, r), f[r]);
        for (ulong m = 0; m < want; m++)
            INVARIANT(numa.get_distance(n, m) == dist[n][m], "distance %lu-%lu is %lu, not %lu", n, m,
                (ulong)numa.get_distance(n, m), dist[n][m]);
    }
    for (ulong i = 0; i < machine.pages; i++)
        INVARIANT(numa.get_phys_node(i * PAGE_BYTES) == topo.page_node[i], "page %lu is node %lu's, not %lu", i,
            numa.get_phys_node(i * PAGE_BYTES), topo.page_node[i]);
    for (ulong c = 0; c <= topo.cpus; c++)
    {
        auto it = topo.cpu_node.find(c);
        const ulong node = (c < topo.cpus && it != topo.cpu_node.end()) ? it->second : 0;
        INVARIANT(numa.get_cpu_node(c) == node, "CPU %lu is node %lu's, not %lu", c, numa.get_cpu_node(c), node);
    }
    fuzz::reached(want == 2 ? "two nodes" : "three nodes");
}

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
    pages.assign(machine.pages, {PageState::Machine, 1, false, false});
    /* zerofree=verify: what an allocation takes as known zero, read */
    pt.set_zero_check(in.chance(4));
    /* What drain_early_free_list leaves: every usable page on node 0's list
       -- the firmware's tables not read yet -- at reference count 1 */
    for (ulong i = machine.pages; i-- > RESERVED_PAGES;)
    {
        machine.page_array[i].init(i * PAGE_BYTES);
        pt.node_free_[0].free_list.insert_head(&machine.page_array[i].list_entry);
        pt.node_free_[0].free_count++;
        pt.free_pages_count_++;
        pages[i].state = PageState::Free;
    }
    for (ulong i = 0; i < RESERVED_PAGES; i++)
        machine.page_array[i].init(i * PAGE_BYTES);

    /* The tables read, the topology sealed, and the free list spread over
       it, as the boot does right after ACPI */
    make_topology(in);
    pt.spread_free_pages();

    /* The root, as Setup allocates it */
    Page* root = pt.alloc_page();
    INVARIANT(root != nullptr, "no page for the root");
    pt.root_ = root->get_phy_address();
    pages[index(pt.root_)].state = PageState::Table;

    /* The pool, right after the free list as the boot reserves it, and its
       window right after it, as the boot makes it -- or later, or never */
    if (machine.pages >= 2 * HUGE_PAGES && in.chance(192))
    {
        reserve_pool(in);
        if (in.chance(160))
            pool_window(in);
    }
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
            if (topo.nodes > 1 && in.chance(64))
                alloc_on(in);
            else
                alloc(in);
            break;
        case 1:
            if (topo.nodes > 1 && in.chance(48))
                move_cpu(in);
            else
                alloc_keeping(in);
            break;
        case 2:
            zero_step(in);
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
                if (in.chance(16))
                    pool_window(in);
                else if (in.boolean())
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
    mmio_known.clear();
    prot.clear();
    pool_blocks.clear();
    pool_zeroed.clear();
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
    Numa& numa = Numa::get_instance();
    numa.~Numa();
    new (&numa) Numa();
    topo = Topology();
}

}

const fuzz::Target fuzz::THE_TARGET = {"pagetable", reset, run, 1024, 20000};
