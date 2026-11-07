#pragma once

#include <lib/stdlib.h>
#include <lib/printer.h>
#include <kernel/spin_lock.h>
#include <kernel/panic.h>
#include <hal/pte.h>

namespace kernel
{

namespace mm
{

struct Page final
{
    void init(ulong addr)
    {
        list_entry.init();
        ref_count.set(1);
        phy_addr = addr;
    }

    /* A page's address is page-aligned, so its low bit is free to say what
       the descriptor has no room for: that the page, free, is zeroed or
       being zeroed -- on the zeroed list, or on its way there from the other
       (PageTable::zero_free_page). Set and cleared under the page table's
       Lock, and only while the page is free: a page handed out never has
       it, so one freed with it is a page freed twice (free_page_no_lock).
       Read and written as relaxed atomics -- plain loads and stores on
       either arch -- because the address is read without the lock
       (check_free_list's snapshot, is_frame_address on an address handed
       in) while the bit may be flipped beside it; the address's own bits
       never change, so whichever word is read, the address is the same. */
    static const ulong ZEROED_BIT = 1;

    void get()
    {
        ref_count.inc();
    }

    void put()
    {
        BUG_ON(ref_count.get() == 0);
        ref_count.dec();
    }

    ulong get_phy_address()
    {
        return __atomic_load_n(&phy_addr, __ATOMIC_RELAXED) & ~ZEROED_BIT;
    }

    bool is_zeroed()
    {
        return (__atomic_load_n(&phy_addr, __ATOMIC_RELAXED) & ZEROED_BIT) != 0;
    }

    void set_zeroed(bool zeroed)
    {
        const ulong addr = __atomic_load_n(&phy_addr, __ATOMIC_RELAXED) & ~ZEROED_BIT;
        __atomic_store_n(&phy_addr, zeroed ? (addr | ZEROED_BIT) : addr, __ATOMIC_RELAXED);
    }

    stdlib::ListEntry list_entry;
    kernel::Atomic ref_count;
    ulong phy_addr;
};

static_assert(sizeof(Page) == 0x20, "Invalid size");
static_assert((consts::PAGE_SIZE % sizeof(Page)) == 0, "Invalid size");

class BuiltinPageTable final
{
public:
    static BuiltinPageTable& get_instance()
    {
        static BuiltinPageTable instance;
        return instance;
    }

    bool setup();

    ulong virt_to_phys(ulong virt_addr);

    ulong phys_to_virt(ulong phy_addr);

    ulong get_root();

    /* One past the highest physical address the bootstrap linear map
       reaches. setup() sets it; it is the hard ceiling on what
       PageTable::get_free_pages can put on the free list, because building
       that list threads a next-pointer through the free pages themselves,
       which means writing to every one of them through this map. */
    ulong get_mapped_limit();

    /* Extend the map over usable RAM above what setup() could reach, and
       raise mapped_limit_ to match. Best effort: on hardware that cannot do
       it the map keeps the extent setup() gave it, which is why there is no
       status to return -- the two mm: lines get_free_pages prints report the
       outcome either way. Must be called once the memory map is populated,
       which on x86 is after the Multiboot tags are parsed and therefore
       after setup(). */
    void map_high_ram();

private:
    BuiltinPageTable(const BuiltinPageTable& other) = delete;
    BuiltinPageTable(BuiltinPageTable&& other) = delete;
    BuiltinPageTable& operator=(const BuiltinPageTable& other) = delete;
    BuiltinPageTable& operator=(BuiltinPageTable&& other) = delete;
    BuiltinPageTable();
    ~BuiltinPageTable();

    PtePage p4_page_ __attribute__((aligned(consts::PAGE_SIZE)));
    PtePage p3_kernel_page_ __attribute__((aligned(consts::PAGE_SIZE)));
    PtePage p3_user_page_ __attribute__((aligned(consts::PAGE_SIZE)));
    PtePage p2_kernel_page_[4] __attribute__((aligned(consts::PAGE_SIZE)));
    PtePage p2_user_page_[4] __attribute__((aligned(consts::PAGE_SIZE)));

    ulong mapped_limit_;
};

class PageTable final
{
public:
    static PageTable& get_instance()
    {
        static PageTable instance;
        return instance;
    }

    bool setup();
    bool setup_free_pages_list();

    ulong get_root();

    static void invalidate_local_tlb();
    static void invalidate_local_tlb_address(ulong virt_addr);
    static void invalidate_local_tlb_range(ulong virt_addr, ulong count);

    ulong tmp_map_page(ulong phy_addr);
    ulong tmp_unmap_page(ulong virt_addr);
    ulong tmp_map_address(ulong phy_addr);
    ulong tmp_map_range(ulong phy_addr, size_t len);

    ulong virt_to_phys(ulong virt_addr);
    ulong virt_to_phys_locked(ulong virt_addr);

    ulong get_free_pages_count();
    /* The fewest pages there have been free since the free list was built:
       how near the machine has come to none, guests' frames and the huge
       pool's reserve included */
    ulong get_free_pages_lowest();
    ulong get_total_pages_count();

    ulong get_va_end();

    Page* get_page(ulong phy_addr);

    bool map_page(ulong virt_addr, Page* page);
    Page* unmap_page(ulong virt_addr);

    /* Range forms of map_page/unmap_page. A run of consecutive VAs shares
       one walk down L4/L3/L2 and one temp mapping of the L1 table, so an
       n-page block costs 4 temp mappings instead of 4n; map_page is the
       n == 1 case. Only the local TLB is invalidated, as with map_page --
       the caller shoots down remote CPUs once for the whole range.

       map_pages takes an array of Page*, map_contiguous_pages a run of Page
       structs, map_phys_pages the physical addresses of pages that already
       exist (it takes the mapping reference itself, like the others).
       All three are all-or-nothing: on failure nothing is left mapped and
       no mapping reference is left outstanding. */
    bool map_pages(ulong virt_addr, Page* const* pages, size_t count);
    bool map_contiguous_pages(ulong virt_addr, Page* pages, size_t count);
    bool map_phys_pages(ulong virt_addr, const ulong* phy_addrs, size_t count);

    /* Drop the mapping reference on count pages; with free_pages set they
       also go back to the free list (the alloc_page/map_pages pairing). */
    void unmap_pages(ulong virt_addr, size_t count, bool free_pages);

    /* Cache policy for map_mmio_region.

       MmioUncached is the only correct choice for device registers: every
       load and store reaches the device, in program order.

       MmioWriteCombining is for memory-like device RAM with no read side
       effects -- a framebuffer. Stores may be buffered, merged and
       reordered, which is what makes drawing affordable; the writer must
       call hal::wc_flush() when the pixels have to be visible, and must
       never use it for registers. Falls back to uncached if the CPU cannot
       do write-combining (hal::is_write_combining_available). */
    enum MmioCachePolicy
    {
        MmioUncached = 0,
        MmioWriteCombining = 1,
    };

    /* Map a physical MMIO range into kernel virtual space.
       phys_addr must be page-aligned.
       Returns kernel virtual address, or 0 on failure.

       The mapping is always non-executable: nothing is ever fetched from
       MMIO, and on real hardware a speculative fetch from device memory can
       trigger read side effects.

       Boot-ordering constraint: the mapping is placed at
       phys_addr + KERNEL_SPACE_BASE outside the VaAllocator, is permanent
       (never unmapped), and only the local TLB is invalidated. Callers
       must therefore run on the BSP before the APs are started
       (currently: HPET and driver init in Main2/bp_startup). */
    ulong map_mmio_region(ulong phys_addr, ulong size_bytes,
        MmioCachePolicy policy = MmioUncached);

    /* Tighten permissions on an existing kernel-image mapping to enforce
       W^X: writable=false makes the range read-only, executable=false makes
       it non-executable (NX/PXN). Walks the 4KiB leaf PTEs in [virt_addr,
       virt_addr+size_bytes) and invalidates the local TLB. */
    bool protect_range(ulong virt_addr, ulong size_bytes, bool writable, bool executable);

    /* Set exactly these permissions on [virt_addr, virt_addr+size_bytes),
       loosening as well as tightening, which protect_range never does. For
       memory the kernel owns outright and hands out as code: a loadable
       module's pages, made executable once written and writable again
       before they are freed. Same walk and same 4KiB-leaf requirement, and
       likewise only the local TLB is invalidated -- the caller shoots down
       the rest (CpuTable::invalidate_tlb_range). */
    bool set_range_protection(ulong virt_addr, ulong size_bytes, bool writable, bool executable);

    /* Walk every page descriptor and report any page that is on the free
       list but must not be: inside a reserved region, inside the kernel
       image, or outside usable RAM. That is the invariant the whole
       memory-map carve-out scheme rests on, and its violation is silent
       until something overwrites page_array_ or an ACPI table. Deliberately
       runs without the lock -- it is a diagnostic, and holding this lock
       across sixteen million pages would stall the machine harder than any
       fault it could find. */
    void check_free_list(stdlib::Printer& printer);

    /* A page, zeroed. Free pages are on one of two lists: the zeroed one,
       pages the zeroing tasks have cleared (zero_free_page), and the other,
       pages as their last owner left them. alloc_page takes from the other
       first and zeroes what it takes -- the page its last owner just let go
       of is still in the cache -- and from the zeroed list when the other is
       empty, where there is nothing to zero. */
    Page* alloc_page();
    /* A page, zeroed, as alloc_page gives one -- or none while the free list
       holds `keep` pages or fewer: what an allocation that must leave the
       rest of the kernel its memory takes (kernel_frame_alloc, a guest's
       RAM). The count is looked at under the lock the page is taken under.
       It takes from the zeroed list first: a guest's memory is gigabytes,
       and every page of it already cleared is one its start does not wait
       for. */
    Page* alloc_page_keeping(ulong keep);

    /* The zeroing tasks' work (PageZeroer), a page at a time so that each
       call is short.

       zero_free_page: a free page off the other list -- its cold end, the
       allocations taking from the other -- zeroed and put on the zeroed one.
       The page is zeroed off Lock, through this CPU's frame slot, and is on
       neither list meanwhile: still counted free, as it is nobody's, and
       marked, so that a free of it -- a page freed twice -- panics.
       Interrupts are off from the take to the put, a microsecond, so that
       nothing keeps it off its list for longer; and a page is taken only
       while more than `keep` pages are free. The zeroing tasks keep the
       kernel's reserve (get_kernel_reserve_pages), where the page off the
       list can make no allocation fail: a guest's stops at the reserve, and
       the kernel's would have to take every page there is in that
       microsecond -- and the one after the last would fail anyway. False
       when there was nothing to zero.

       zero_free_huge_page: a page of a free block of the huge pool zeroed,
       under the pool's lock, the block staying free throughout. With all 512
       pages done the block is known zero, and alloc_huge_frame hands it out
       as it is; with no block known zero, an allocation meanwhile takes this
       one first and zeroes only the pages not done yet, and the zeroing
       starts on another -- so that while a guest takes its memory, the
       zeroing on an idle CPU does part of each block's work rather than
       work the allocation does again. The pool's lock is its own so that
       the zeroing keeps no 4 KiB allocation waiting. False when every free
       block is known zero, or there is no pool.

       get_zeroed_pages_count, get_huge_pool_zeroed: how many free pages, and
       free blocks, are known zero.

       set_zero_check (zerofree=verify): every page, and block, an allocation
       takes as known zero read first, and a word that is not zero a panic
       naming the page -- something wrote to a free page, a use after free,
       caught at the allocation rather than handed to a guest. Set once, at
       boot, before there is another CPU. */
    bool zero_free_page(ulong keep);
    bool zero_free_huge_page();
    ulong get_zeroed_pages_count();
    ulong get_huge_pool_zeroed();
    void set_zero_check(bool on);
    bool is_zero_check();
    static const ulong MAX_CONTIGUOUS_PAGES = 128;

    /* The most pages mm::map_large_pages maps at one run of VA: 16 MiB, for
       what outgrows the largest block map_pages serves -- a loadable
       module's image, the .ko file it is read from */
    static const ulong MAX_LARGE_MAP_PAGES = 4096;
    /* A run of count pages, physically contiguous, zeroed -- found and
       taken off the free lists under Lock, then zeroed off it a page at a
       time, as alloc_page zeroes one, the pages known zero left as they
       are -- or nullptr for none. count is 1 to MAX_CONTIGUOUS_PAGES. */
    Page* alloc_contiguous_pages(ulong count);
    void free_page(Page* page);

    /* Frames: pages handed out by address and mapped nowhere -- not into
       the kernel's space, nor into any table but whatever their owner puts
       them in (a guest's nested page table). The owner is a Rust value
       (kcore::frame::Frame) on the far side of the C ABI, which is why a
       frame is named by its physical address rather than its descriptor,
       and why every address that comes back is checked.

       is_frame_address: whether phy_addr is page-aligned and has a descriptor,
       without the panic get_page has for one that does not. Takes no
       reference.

       free_frame: back onto the free list. A page that is on the list
       already -- a frame freed twice -- is a panic here rather than a free
       list that loops.

       map_frame_slot: this CPU's own slot of the tmp_map window, mapped onto
       the frame at phy_addr for one copy into or out of it, and the slot's
       address; unmap_frame_slot clears it again, its TLB entry with it. With
       interrupts off from the map to the unmap -- the caller's -- the slot
       is that copy's alone, which is what lets it go without tmp_map_lock_,
       the slot search and the reference tmp_map_page takes: the frame is the
       caller's for the length of the call. RAM, so write-back; data, so
       no-execute. */
    bool is_frame_address(ulong phy_addr);
    void free_frame(ulong phy_addr);
    ulong map_frame_slot(ulong phy_addr);
    void unmap_frame_slot(ulong virt_addr);

    /* The pages no allocation for a guest may take: what the kernel goes on
       needing once guests have the rest -- its heap, the pages their nested
       tables are made of, a module loaded later. A sixteenth of the
       machine, and no less than 64 MiB. Without it a guest given more
       memory than the machine had free took the last page, and the next
       allocation anywhere that may not fail -- a plain `new`, a Rust Box --
       panicked the host under every other guest. kernel_frame_alloc keeps
       it free; reserve_huge_pool keeps twice it, the kernel's own boot being
       still to come out of what it leaves. */
    static const ulong KERNEL_RESERVE_MIN_BYTES = 64 * consts::MB;
    static const ulong KERNEL_RESERVE_FRACTION = 16;
    ulong get_kernel_reserve_pages();

    /* The huge pool (hugepages=): 2MiB blocks -- physically contiguous runs
       of HUGE_FRAME_PAGES pages, aligned to their size -- reserved at boot for
       guests' memory, so that a guest's nested table maps each with one
       entry, and its TLB holds 2MiB where it held 4KiB. Taken off the free
       list right after setup_free_pages_list, before anything has had the
       chance to fragment it, and kept out of every other allocation for as
       long as the machine is up: a block of the pool is on no free list and
       in no free_pages_count_, and its pages' descriptors are as a page handed
       out has them (self-pointing), so alloc_contiguous_pages and
       check_free_list take none of them for free. What is in the pool and what
       of it is free are two bitmaps of their own, not the descriptors' list
       entries, which those two read as "on the free list".

       reserve_huge_pool: once, at boot, with the bytes hugepages= asks for.
       Takes whole free blocks, the highest first -- low memory is what the
       kernel and a device that addresses 32 bits want -- as many as that is,
       and never so many that fewer than twice get_kernel_reserve_pages pages
       are left free. How many blocks it took. reserve_huge_pool_keeping is the
       same with the pages to leave free said: what the page table fuzzer's
       machine of a few MiB calls it with.

       alloc_huge_frame: a block of the pool, zeroed as alloc_page zeroes a
       page -- what a guest finds in its memory is what it was given, never
       what the last guest left there: its physical address, or 0 when none
       is free. A block known zero (zero_free_huge_page) first, as it is;
       else the block the zeroing is part way through, of which only the
       pages it has not done are zeroed here; else any free one, zeroed
       here.

       free_huge_frame: a block back into the pool. An address that is no
       block of the pool, or one already back, is a panic, as free_frame's
       double free is; free_frame of a page inside a block is one too, the
       free list and the pool never sharing a page.

       get_huge_pool_blocks, get_huge_pool_free: how many blocks the pool has, and
       how many of them are free. */
    static const ulong HUGE_FRAME_PAGES = 512;
    static const ulong HUGE_FRAME_SIZE = HUGE_FRAME_PAGES * consts::PAGE_SIZE;
    ulong reserve_huge_pool(ulong bytes);
    ulong reserve_huge_pool_keeping(ulong bytes, ulong keep_pages);
    ulong alloc_huge_frame();
    void free_huge_frame(ulong phy_addr);
    ulong get_huge_pool_blocks();
    ulong get_huge_pool_free();

    /* The huge pool's window: every block of the pool mapped with one 2MiB
       entry at HUGE_POOL_WINDOW plus its physical address -- RAM, so
       write-back; data, so no-execute -- from map_huge_pool_window, right
       after reserve_huge_pool, for as long as the machine is up. What
       kernel_frame_read and the rest copy a guest's memory through: through
       a CPU's frame slot every copy is a slot mapped and unmapped and its
       TLB entry flushed, interrupts off for the copy -- on the EX44's E-cores
       a guest disk's 256KiB read took 65 us to give back, a microsecond a
       page, and every request's header, status and descriptors are copies
       of their own. The pool is the guests' memory and nothing else: a block is
       handed out zeroed (alloc_huge_frame), so what the window holds of one
       not handed out is a guest's that has gone, as a freed frame's is.

       map_huge_pool_window: the window made, once, before there is another
       CPU; all of it or none -- a table that cannot be had, or the window's
       top-level entry taken already, unmaps what was made and leaves the
       copies to the frame slots, said. Whether it is made. huge_pool_window:
       phy_addr's address in the window, or 0 when there is no window or
       phy_addr is in no block of the pool. */
    bool map_huge_pool_window();
    ulong huge_pool_window(ulong phy_addr);

private:
    PageTable(const PageTable& other) = delete;
    PageTable(PageTable&& other) = delete;
    PageTable& operator=(const PageTable& other) = delete;
    PageTable& operator=(PageTable&& other) = delete;
    PageTable();
    ~PageTable();

    ulong get_free_page();

    /* Move every page on the early physical free list onto free_pages_list_. */
    void drain_early_free_list();

    /* One 4KiB leaf in the table setup() is building, before it is live.
       executable is for the kernel image alone -- the kernel runs out of
       that mapping from the root switch until protect_range refines it;
       every other page setup() maps is data and gets NX. */
    bool setup_page(ulong virt_addr, ulong phy_addr, bool executable);

    /* setup_page stopping one level short: a 2MiB leaf at L2 instead of a
       4KiB one at L1. Only page_array_ uses it, and only because it is the one
       runtime mapping large enough for the page size to matter. */
    bool setup_huge_page(ulong virt_addr, ulong phy_addr);

    /* Find a physically contiguous, huge-page-aligned home for page_array_ at
       or above lower_bound, clear of the kernel image and of every reserved
       region. Returns 0 if the machine's map has no window that fits. */
    ulong reserve_page_array(ulong bytes, ulong lower_bound);

    /* Build the early physical free list from the memory map. Pages below
       exclude_limit go on the side list instead (see the comment there). */
    bool get_free_pages(ulong exclude_limit);

    Page* alloc_page_no_lock();
    void free_page_no_lock(Page* page);
    /* The halves of an allocation: a page off a free list, with Lock held --
       the zeroed one first when zeroed_first, the other first otherwise --
       its mark cleared and `zeroed` saying which list it came off; and its
       zeroing, which needs no lock -- the page is the caller's alone once it
       is off the list -- unless it came off the zeroed one. */
    Page* take_free_page_no_lock(bool zeroed_first, bool& zeroed);
    void zero_page(Page* page);
    /* What an allocation does with the page it took: zeroes it, unless it is
       known zero -- and then reads it, under zero_check_ */
    void zero_unless_zeroed(Page* page, bool zeroed);
    /* The page at va, mapped, read: a word that is not zero is a panic */
    void check_zeroed(ulong va, ulong phy_addr);
    /* A page taken off a free list by anything that does not hand it out
       whole -- a run, a block for the pool: off its list, its mark and the
       counts with it. Under Lock. */
    void unlist_free_page_no_lock(Page* page);
    /* alloc_contiguous_pages' half under Lock: a run of count free pages
       found and taken off the lists, zeroed[j] saying whether page j of it
       was known zero; nullptr when there is none. examined counts the list
       entries looked at. */
    Page* take_run_no_lock(ulong count, bool* zeroed, ulong& examined);

    /* Where a range map takes its frames from: exactly one member is set.
       A tagged source keeps one range walk serving all three public forms
       -- this kernel has no lambdas to parameterize it with. */
    struct MapSource
    {
        Page* const* ptrs;
        Page* array;
        const ulong* phy_addrs;
    };

    Page* source_page(const MapSource& src, size_t index);

    PtePage* walk_to_l1_locked(ulong virt_addr, bool create);
    bool map_range_locked(ulong virt_addr, size_t count, const MapSource& src);

    bool change_range_protection(ulong virt_addr, ulong size_bytes, bool writable,
        bool executable, bool exact);
    void unmap_range_locked(ulong virt_addr, size_t count, bool free_pages);

    ulong tmp_map_start_;
    kernel::SpinLock tmp_map_lock_;
    PtePage *tmp_map_l1_page_;

    /* The tmp_map window, TMP_MAP_PAGE_COUNT slots in one L1 table: first the
       ones any CPU takes under tmp_map_lock_ (tmp_map_page, tmp_map_range), then a
       slot for each CPU, for copying a frame (map_frame_slot). */
    static const size_t TMP_MAP_PAGE_COUNT = 512;
    static const size_t FRAME_SLOT_COUNT = 64;
    static const size_t TMP_MAP_SHARED_COUNT = TMP_MAP_PAGE_COUNT - FRAME_SLOT_COUNT;
    Page *tmp_map_page_array_[TMP_MAP_SHARED_COUNT];

    ulong frame_slot_address(ulong cpu);

    ulong get_l1_page(ulong virt_addr);

    ulong root_;

    SpinLock lock_;
    SpinLock free_pages_lock_;
    ulong free_pages_;
    /* Usable RAM below the page_array_ limit, parked by ExcludeFreePages so
       Setup never allocates it; reclaimed in setup_free_pages_list. */
    ulong excluded_pages_;

    /* page_array_ is one Page per 4KiB frame of physical memory, so on a big
       machine it is hundreds of megabytes and every free-list walk touches
       it at random. It gets a physically contiguous home of its own, mapped
       with 2MiB pages: 512 times fewer leaf entries, and a TLB that can
       actually cover it. */
    static const ulong HUGE_PAGE_SIZE = 512 * consts::PAGE_SIZE;

    Page* page_array_;
    ulong page_array_phys_;
    ulong page_array_count_;
    ulong highest_phy_addr_;
    /* The free pages: free_pages_list_ those as their last owner left them,
       zeroed_pages_list_ those known to read as zeros, each of them marked
       (Page::ZEROED_BIT). free_pages_count_ counts both lists and a page
       zero_free_page has off them on its way from one to the other;
       zeroed_pages_count_ the zeroed list alone. */
    stdlib::ListEntry free_pages_list_;
    stdlib::ListEntry zeroed_pages_list_;
    ulong free_pages_count_;
    /* The least free_pages_count_ has been since setup_free_pages_list:
       lowered where the count is, under Lock */
    ulong free_pages_lowest_;
    ulong zeroed_pages_count_;
    ulong total_pages_count_;

    /* The huge pool's bitmaps, a bit a 2MiB block of physical memory: in
       the pool, written only by reserve_huge_pool before there is another
       CPU; free in it; and known zero, of what is free. Sized for the most
       RAM the bootstrap map can reach on either arch, one PDPT of 1GiB
       blocks: 512 GiB, 32 KiB a bitmap. */
    static const ulong HUGE_POOL_MAX_BYTES = 512 * consts::GB;
    static const ulong HUGE_POOL_MAX_BLOCKS = HUGE_POOL_MAX_BYTES / HUGE_FRAME_SIZE;
    static const ulong HUGE_POOL_WORD_BITS = 8 * sizeof(ulong);
    static const ulong HUGE_POOL_WORDS = HUGE_POOL_MAX_BLOCKS / HUGE_POOL_WORD_BITS;
    /* What of the pool is free and what of that is known zero, and their
       counts, the zeroing's place and the hint: under huge_pool_lock_, which
       nests inside Lock (reserve_huge_pool takes both) and is never taken
       the other way round. */
    kernel::SpinLock huge_pool_lock_;
    ulong huge_pool_in_[HUGE_POOL_WORDS];
    ulong huge_pool_free_bits_[HUGE_POOL_WORDS];
    /* Free and known zero, a subset of the free bits */
    ulong huge_pool_zeroed_bits_[HUGE_POOL_WORDS];
    ulong huge_pool_count_;
    ulong huge_pool_free_count_;
    ulong huge_pool_zeroed_count_;
    /* The block zero_free_huge_page is zeroing and the page of it it does
       next: NO_HUGE_BLOCK when none -- and set to that by alloc_huge_frame
       when it takes this one, which zeroes only the pages from
       huge_zero_next_ on: those before it are zeros, and the new owner's
       from then on. */
    static const ulong NO_HUGE_BLOCK = ~0UL;
    ulong huge_zero_block_;
    ulong huge_zero_next_;
    /* The word alloc_huge_frame looks in first: where the last block came
       from, so that a guest taking thousands of blocks does not scan past
       the ones it took already for each. */
    ulong huge_pool_hint_;

    /* The block phy_addr is in, when it is one the pool has: false for an
       address past the bitmaps or in no block of the pool. */
    bool huge_pool_block_of(ulong phy_addr, ulong& block);
    static bool huge_pool_test(const ulong* bits, ulong block);
    static void huge_pool_set(ulong* bits, ulong block, bool value);
    bool huge_block_free(ulong block);
    /* A block whose bit is set in bits and not in minus (nullptr: none), the
       search from the hint's word on. Under huge_pool_lock_. */
    bool huge_pool_find(const ulong* bits, const ulong* minus, ulong& block);
    /* A page of the pool zeroed, and read, through this CPU's frame slot,
       as kernel_frame_read copies: interrupts off for that page's stores
       and no longer, and no tmp_map_lock_ for each of the 512 */
    void zero_huge_page(ulong phy_addr);
    void check_huge_page_zeroed(ulong phy_addr);

    /* Where the window is: a top-level entry of its own, 64 TiB into the
       kernel's half -- far past the image, page_array_ and the allocators'
       ranges, which a machine's RAM sizes -- and as big as the pool may be. */
    static const ulong HUGE_POOL_WINDOW = 0xFFFFC00000000000UL;
    static const ulong TOP_ENTRY_SPAN = 512 * consts::GB;
    static_assert(HUGE_POOL_WINDOW % TOP_ENTRY_SPAN == 0, "the window starts a top-level entry");
    static_assert(HUGE_POOL_MAX_BYTES <= TOP_ENTRY_SPAN, "the pool fits one top-level entry");
    /* Set once by map_huge_pool_window, before there is another CPU, and
       never again: read without the lock. */
    bool huge_pool_window_on_;
    /* zerofree=verify (set_zero_check) */
    bool zero_check_;

    /* The L2 table that maps virt_addr, temp-mapped -- the levels above it
       made where they are missing -- or nullptr: no page for a table, a
       1GiB block where a table belongs, or a full tmp_map window. Lock must
       be held, and the caller tmp_unmap_page()s the result. */
    PtePage* walk_to_l2_locked(ulong virt_addr);
    /* A window cut short undone: every table under its top-level entry --
       all the window's own, the entry having been empty -- freed, and the
       entry cleared. Lock must be held. */
    void unmap_huge_pool_window_locked();
};

}
}