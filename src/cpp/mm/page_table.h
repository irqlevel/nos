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
        return phy_addr;
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
       reaches. Setup() sets it; it is the hard ceiling on what
       PageTable::GetFreePages can put on the free list, because building
       that list threads a next-pointer through the free pages themselves,
       which means writing to every one of them through this map. */
    ulong get_mapped_limit();

    /* Extend the map over usable RAM above what Setup() could reach, and
       raise MappedLimit to match. Best effort: on hardware that cannot do
       it the map keeps the extent Setup() gave it, which is why there is no
       status to return -- the two mm: lines GetFreePages prints report the
       outcome either way. Must be called once the memory map is populated,
       which on x86 is after the Multiboot tags are parsed and therefore
       after Setup(). */
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
    ulong get_total_pages_count();

    ulong get_va_end();

    Page* get_page(ulong phy_addr);

    bool map_page(ulong virt_addr, Page* page);
    Page* unmap_page(ulong virt_addr);

    /* Range forms of MapPage/UnmapPage. A run of consecutive VAs shares
       one walk down L4/L3/L2 and one temp mapping of the L1 table, so an
       n-page block costs 4 temp mappings instead of 4n; MapPage is the
       n == 1 case. Only the local TLB is invalidated, as with MapPage --
       the caller shoots down remote CPUs once for the whole range.

       MapPages takes an array of Page*, MapContiguousPages a run of Page
       structs, MapPhysPages the physical addresses of pages that already
       exist (it takes the mapping reference itself, like the others).
       All three are all-or-nothing: on failure nothing is left mapped and
       no mapping reference is left outstanding. */
    bool map_pages(ulong virt_addr, Page* const* pages, size_t count);
    bool map_contiguous_pages(ulong virt_addr, Page* pages, size_t count);
    bool map_phys_pages(ulong virt_addr, const ulong* phy_addrs, size_t count);

    /* Drop the mapping reference on count pages; with freePages set they
       also go back to the free list (the AllocPage/MapPages pairing). */
    void unmap_pages(ulong virt_addr, size_t count, bool free_pages);

    /* Cache policy for MapMmioRegion.

       MmioUncached is the only correct choice for device registers: every
       load and store reaches the device, in program order.

       MmioWriteCombining is for memory-like device RAM with no read side
       effects -- a framebuffer. Stores may be buffered, merged and
       reordered, which is what makes drawing affordable; the writer must
       call Hal::WcFlush() when the pixels have to be visible, and must
       never use it for registers. Falls back to uncached if the CPU cannot
       do write-combining (Hal::IsWriteCombiningAvailable). */
    enum MmioCachePolicy
    {
        MmioUncached = 0,
        MmioWriteCombining = 1,
    };

    /* Map a physical MMIO range into kernel virtual space.
       physAddr must be page-aligned.
       Returns kernel virtual address, or 0 on failure.

       The mapping is always non-executable: nothing is ever fetched from
       MMIO, and on real hardware a speculative fetch from device memory can
       trigger read side effects.

       Boot-ordering constraint: the mapping is placed at
       physAddr + KernelSpaceBase outside the VaAllocator, is permanent
       (never unmapped), and only the local TLB is invalidated. Callers
       must therefore run on the BSP before the APs are started
       (currently: HPET and driver init in Main2/BpStartup). */
    ulong map_mmio_region(ulong phys_addr, ulong size_bytes,
        MmioCachePolicy policy = MmioUncached);

    /* Tighten permissions on an existing kernel-image mapping to enforce
       W^X: writable=false makes the range read-only, executable=false makes
       it non-executable (NX/PXN). Walks the 4KiB leaf PTEs in [virtAddr,
       virtAddr+sizeBytes) and invalidates the local TLB. */
    bool protect_range(ulong virt_addr, ulong size_bytes, bool writable, bool executable);

    /* Set exactly these permissions on [virtAddr, virtAddr+sizeBytes),
       loosening as well as tightening, which ProtectRange never does. For
       memory the kernel owns outright and hands out as code: a loadable
       module's pages, made executable once written and writable again
       before they are freed. Same walk and same 4KiB-leaf requirement, and
       likewise only the local TLB is invalidated -- the caller shoots down
       the rest (CpuTable::InvalidateTlbRange). */
    bool set_range_protection(ulong virt_addr, ulong size_bytes, bool writable, bool executable);

    /* Walk every page descriptor and report any page that is on the free
       list but must not be: inside a reserved region, inside the kernel
       image, or outside usable RAM. That is the invariant the whole
       memory-map carve-out scheme rests on, and its violation is silent
       until something overwrites PageArray or an ACPI table. Deliberately
       runs without the lock -- it is a diagnostic, and holding this lock
       across sixteen million pages would stall the machine harder than any
       fault it could find. */
    void check_free_list(stdlib::Printer& printer);

    Page* alloc_page();
    /* A page, zeroed, as AllocPage gives one -- or none while the free list
       holds `keep` pages or fewer: what an allocation that must leave the
       rest of the kernel its memory takes (kernel_frame_alloc, a guest's
       RAM). The count is looked at under the lock the page is taken under. */
    Page* alloc_page_keeping(ulong keep);
    static const ulong MAX_CONTIGUOUS_PAGES = 128;

    /* The most pages Mm::MapLargePages maps at one run of VA: 16 MiB, for
       what outgrows the largest block MapPages serves -- a loadable
       module's image, the .ko file it is read from */
    static const ulong MAX_LARGE_MAP_PAGES = 4096;
    Page* alloc_contiguous_pages(ulong count);
    void free_page(Page* page);

    /* Frames: pages handed out by address and mapped nowhere -- not into
       the kernel's space, nor into any table but whatever their owner puts
       them in (a guest's nested page table). The owner is a Rust value
       (kcore::frame::Frame) on the far side of the C ABI, which is why a
       frame is named by its physical address rather than its descriptor,
       and why every address that comes back is checked.

       IsFrameAddress: whether phyAddr is page-aligned and has a descriptor,
       without the panic GetPage has for one that does not. Takes no
       reference.

       FreeFrame: back onto the free list. A page that is on the list
       already -- a frame freed twice -- is a panic here rather than a free
       list that loops.

       MapFrameSlot: this CPU's own slot of the TmpMap window, mapped onto
       the frame at phyAddr for one copy into or out of it, and the slot's
       address; UnmapFrameSlot clears it again, its TLB entry with it. With
       interrupts off from the map to the unmap -- the caller's -- the slot
       is that copy's alone, which is what lets it go without TmpMapLock,
       the slot search and the reference TmpMapPage takes: the frame is the
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
       it free; ReserveHugePool keeps twice it, the kernel's own boot being
       still to come out of what it leaves. */
    static const ulong KERNEL_RESERVE_MIN_BYTES = 64 * consts::MB;
    static const ulong KERNEL_RESERVE_FRACTION = 16;
    ulong get_kernel_reserve_pages();

    /* The huge pool (hugepages=): 2MiB blocks -- physically contiguous runs
       of HugeFramePages pages, aligned to their size -- reserved at boot for
       guests' memory, so that a guest's nested table maps each with one
       entry, and its TLB holds 2MiB where it held 4KiB. Taken off the free
       list right after SetupFreePagesList, before anything has had the
       chance to fragment it, and kept out of every other allocation for as
       long as the machine is up: a block of the pool is on no free list and
       in no FreePagesCount, and its pages' descriptors are as a page handed
       out has them (self-pointing), so AllocContiguousPages and
       CheckFreeList take none of them for free. What is in the pool and what
       of it is free are two bitmaps of their own, not the descriptors' list
       entries, which those two read as "on the free list".

       ReserveHugePool: once, at boot, with the bytes hugepages= asks for.
       Takes whole free blocks, the highest first -- low memory is what the
       kernel and a device that addresses 32 bits want -- as many as that is,
       and never so many that fewer than twice GetKernelReservePages pages
       are left free. How many blocks it took. ReserveHugePoolKeeping is the
       same with the pages to leave free said: what the page table fuzzer's
       machine of a few MiB calls it with.

       AllocHugeFrame: a block of the pool, zeroed as AllocPage zeroes a
       page -- what a guest finds in its memory is what it was given, never
       what the last guest left there: its physical address, or 0 when none
       is free.

       FreeHugeFrame: a block back into the pool. An address that is no
       block of the pool, or one already back, is a panic, as FreeFrame's
       double free is; FreeFrame of a page inside a block is one too, the
       free list and the pool never sharing a page.

       GetHugePoolBlocks, GetHugePoolFree: how many blocks the pool has, and
       how many of them are free. */
    static const ulong HUGE_FRAME_PAGES = 512;
    static const ulong HUGE_FRAME_SIZE = HUGE_FRAME_PAGES * consts::PAGE_SIZE;
    ulong reserve_huge_pool(ulong bytes);
    ulong reserve_huge_pool_keeping(ulong bytes, ulong keep_pages);
    ulong alloc_huge_frame();
    void free_huge_frame(ulong phy_addr);
    ulong get_huge_pool_blocks();
    ulong get_huge_pool_free();

private:
    PageTable(const PageTable& other) = delete;
    PageTable(PageTable&& other) = delete;
    PageTable& operator=(const PageTable& other) = delete;
    PageTable& operator=(PageTable&& other) = delete;
    PageTable();
    ~PageTable();

    ulong get_free_page();

    /* Move every page on the early physical free list onto FreePagesList. */
    void drain_early_free_list();

    /* One 4KiB leaf in the table Setup() is building, before it is live.
       executable is for the kernel image alone -- the kernel runs out of
       that mapping from the root switch until ProtectRange refines it;
       every other page Setup() maps is data and gets NX. */
    bool setup_page(ulong virt_addr, ulong phy_addr, bool executable);

    /* SetupPage stopping one level short: a 2MiB leaf at L2 instead of a
       4KiB one at L1. Only PageArray uses it, and only because it is the one
       runtime mapping large enough for the page size to matter. */
    bool setup_huge_page(ulong virt_addr, ulong phy_addr);

    /* Find a physically contiguous, huge-page-aligned home for PageArray at
       or above lowerBound, clear of the kernel image and of every reserved
       region. Returns 0 if the machine's map has no window that fits. */
    ulong reserve_page_array(ulong bytes, ulong lower_bound);

    /* Build the early physical free list from the memory map. Pages below
       excludeLimit go on the side list instead (see the comment there). */
    bool get_free_pages(ulong exclude_limit);

    Page* alloc_page_no_lock();
    void free_page_no_lock(Page* page);
    /* The halves of an allocation: a page off the free list, with Lock
       held; and its zeroing, which needs no lock -- the page is the
       caller's alone once it is off the list. */
    Page* take_free_page_no_lock();
    void zero_page(Page* page);

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

    /* The TmpMap window, TmpMapPageCount slots in one L1 table: first the
       ones any CPU takes under TmpMapLock (TmpMapPage, TmpMapRange), then a
       slot for each CPU, for copying a frame (MapFrameSlot). */
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
    /* Usable RAM below the PageArray limit, parked by ExcludeFreePages so
       Setup never allocates it; reclaimed in SetupFreePagesList. */
    ulong excluded_pages_;

    /* PageArray is one Page per 4KiB frame of physical memory, so on a big
       machine it is hundreds of megabytes and every free-list walk touches
       it at random. It gets a physically contiguous home of its own, mapped
       with 2MiB pages: 512 times fewer leaf entries, and a TLB that can
       actually cover it. */
    static const ulong HUGE_PAGE_SIZE = 512 * consts::PAGE_SIZE;

    Page* page_array_;
    ulong page_array_phys_;
    ulong page_array_count_;
    ulong highest_phy_addr_;
    stdlib::ListEntry free_pages_list_;
    ulong free_pages_count_;
    ulong total_pages_count_;

    /* The huge pool's two bitmaps, a bit a 2MiB block of physical memory:
       in the pool, written only by ReserveHugePool before there is another
       CPU, and free in it, under Lock. Sized for the most RAM the bootstrap
       map can reach on either arch, one PDPT of 1GiB blocks: 512 GiB, 32
       KiB a bitmap. */
    static const ulong HUGE_POOL_MAX_BYTES = 512 * consts::GB;
    static const ulong HUGE_POOL_MAX_BLOCKS = HUGE_POOL_MAX_BYTES / HUGE_FRAME_SIZE;
    static const ulong HUGE_POOL_WORD_BITS = 8 * sizeof(ulong);
    static const ulong HUGE_POOL_WORDS = HUGE_POOL_MAX_BLOCKS / HUGE_POOL_WORD_BITS;
    ulong huge_pool_in_[HUGE_POOL_WORDS];
    ulong huge_pool_free_bits_[HUGE_POOL_WORDS];
    ulong huge_pool_count_;
    ulong huge_pool_free_count_;
    /* The word AllocHugeFrame looks in first: where the last block came
       from, so that a guest taking thousands of blocks does not scan past
       the ones it took already for each. */
    ulong huge_pool_hint_;

    /* The block phyAddr is in, when it is one the pool has: false for an
       address past the bitmaps or in no block of the pool. */
    bool huge_pool_block_of(ulong phy_addr, ulong& block);
    static bool huge_pool_test(const ulong* bits, ulong block);
    static void huge_pool_set(ulong* bits, ulong block, bool value);
    bool huge_block_free(ulong block);
};

}
}