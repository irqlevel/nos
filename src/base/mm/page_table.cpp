#include "page_table.h"
#include "memory_map.h"

#include <kernel/klog.h>
#include <hal/cpu.h>
#include <hal/irqchip.h>
#include <hal/mmu.h>
#include <kernel/cpu.h>
#include <kernel/debug.h>
#include <kernel/preempt.h>

namespace kernel
{

namespace mm
{

BuiltinPageTable::BuiltinPageTable()
    : mapped_limit_(0)
{
    stdlib::mem_set(&p4_page_, 0, sizeof(p4_page_));

    stdlib::mem_set(&p3_kernel_page_, 0, sizeof(p3_kernel_page_));
    stdlib::mem_set(&p3_user_page_, 0, sizeof(p3_user_page_));

    stdlib::mem_set(&p2_kernel_page_[0], 0, sizeof(p2_kernel_page_));
    stdlib::mem_set(&p2_user_page_[0], 0, sizeof(p2_user_page_));

    klog(0, "PageTable 0x%p p4_page_ 0x%p", this, &p4_page_);
}

ulong BuiltinPageTable::virt_to_phys(ulong virt_addr)
{

    BUG_ON(virt_addr > MemoryMap::USER_SPACE_MAX && virt_addr < MemoryMap::KERNEL_SPACE_BASE);

    if (virt_addr <= MemoryMap::USER_SPACE_MAX)
        return virt_addr;

    return virt_addr - MemoryMap::KERNEL_SPACE_BASE;
}

ulong BuiltinPageTable::phys_to_virt(ulong phy_addr)
{

    return phy_addr + MemoryMap::KERNEL_SPACE_BASE;
}

ulong BuiltinPageTable::get_root()
{
    return virt_to_phys((ulong)&p4_page_);
}

ulong BuiltinPageTable::get_mapped_limit()
{
    return mapped_limit_;
}

BuiltinPageTable::~BuiltinPageTable()
{
}

PageTable::PageTable()
    : root_(0)
    , excluded_pages_(0)
    , page_array_(nullptr)
    , page_array_phys_(0)
    , page_array_count_(0)
    , highest_phy_addr_(0)
    , free_pages_count_(0)
    , free_pages_lowest_(0)
    , zeroed_pages_count_(0)
    , total_pages_count_(0)
    , huge_pool_count_(0)
    , huge_pool_free_count_(0)
    , huge_pool_zeroed_count_(0)
    , huge_zero_block_(NO_HUGE_BLOCK)
    , huge_zero_next_(0)
    , huge_pool_hint_(0)
    , huge_pool_window_on_(false)
    , mmio_next_(MMIO_WINDOW)
    , mmio_mapping_count_(0)
    , zero_check_(false)
{
    klog(0, "PageTable 0x%p", this);
    for (size_t i = 0; i < stdlib::array_size(tmp_map_page_array_); i++)
    {
        tmp_map_page_array_[i] = nullptr;
    }
    tmp_map_l1_page_ = nullptr;
    for (ulong i = 0; i < HUGE_POOL_WORDS; i++)
    {
        huge_pool_in_[i] = 0;
        huge_pool_free_bits_[i] = 0;
        huge_pool_zeroed_bits_[i] = 0;
    }
}

PageTable::~PageTable()
{
}

bool PageTable::get_free_pages(ulong exclude_limit)
{
    auto& mmap = MemoryMap::get_instance();

    /* Free-listing a page means writing a next-pointer into it, so the list
       can only hold what the bootstrap linear map reaches. RAM above that
       is real, reported by the firmware, and unusable to us. */
    const ulong builtin_map_limit = BuiltinPageTable::get_instance().get_mapped_limit();
    BUG_ON(builtin_map_limit == 0);

    /* A 64 GiB machine spends seconds in this loop and seconds more in the
       drain, and to a log read after the fact silence and a hang look the
       same. One line per 8 GiB is eight of them on the largest machine this
       kernel can address and none at all on a small one. */
    const ulong progress_pages = (8 * consts::GB) / consts::PAGE_SIZE;
    const ulong usable_mib = mmap.get_usable_ram_bytes() / consts::MB;
    ulong next_progress = progress_pages;

    /* Hoisted: the kernel image is one physical range, and comparing against
       it beat calling phys_to_virt twice per page. */
    const ulong kernel_phys_start = BuiltinPageTable::get_instance().virt_to_phys(mmap.get_kernel_start());
    const ulong kernel_phys_end = BuiltinPageTable::get_instance().virt_to_phys(mmap.get_kernel_end());

    for (size_t i = 0; i < mmap.get_region_count(); i++)
    {
        ulong addr, len, type;

        if (!mmap.get_region(i, addr, len, type))
            return false;

        if (type != MemoryMap::USABLE_RAM_TYPE)
            continue;

        ulong mem_start = stdlib::round_up(addr, consts::PAGE_SIZE);
        ulong mem_end = ((addr + len) / consts::PAGE_SIZE) * consts::PAGE_SIZE;

        if (mem_start < consts::MB)
            mem_start = stdlib::round_up(consts::MB, consts::PAGE_SIZE);

        if (mem_end > builtin_map_limit)
            mem_end = builtin_map_limit;

        klog(PAGE_ALLOCATOR_LL, "Phy mem_start 0x%lX mem_end 0x%lX", mem_start, mem_end);

        if (mem_start >= mem_end)
            continue;

        klog(PAGE_ALLOCATOR_LL, "get_free_pages: region 0x%lX-0x%lX", mem_start, mem_end);

        ulong address = mem_start;
        while (address < mem_end)
        {
            /* RAM an earlier region of the map listed already: firmware may
               name the same range twice, or two that overlap, and a page put
               on this list a second time makes a loop of it -- its link
               written over with a head that leads back to it. Skipped, and
               not counted again. */
            ulong listed_end = mmap.get_listed_end(i, address);
            if (listed_end != 0)
            {
                address = (listed_end < mem_end) ? listed_end : mem_end;
                continue;
            }

            /* Reserved carve-outs (the arm64 DTB, page_array_'s backing, the
               firmware's own) overlap usable-RAM regions; free-listing them
               would clobber their contents. Skipped a run at a time rather
               than a page at a time -- there are a handful of boundaries and
               millions of pages. */
            ulong reserved_end = mmap.get_reserved_end(address);
            if (reserved_end != 0)
            {
                if (reserved_end > mem_end)
                    reserved_end = mem_end;

                total_pages_count_ += (reserved_end - address) / consts::PAGE_SIZE;
                address = reserved_end;
                continue;
            }

            ulong run_end = mmap.get_next_reserved_start(address, mem_end);
            run_end = mmap.get_next_listed_start(i, address, run_end);
            for (; address < run_end; address += consts::PAGE_SIZE)
            {
                total_pages_count_++;

                if (total_pages_count_ >= next_progress)
                {
                    klog(0, "mm: free list, %lu of %lu MiB",
                        (total_pages_count_ * consts::PAGE_SIZE) / consts::MB, usable_mib);
                    next_progress += progress_pages;
                }

                if (address >= kernel_phys_start && address < kernel_phys_end)
                    continue;

                /* Pages whose identity address is shadowed by the kernel's
                   own VA window go on a side list rather than the main one:
                   they are ordinary usable RAM, but Setup reaches its
                   allocations through the bootstrap map, and these are
                   exactly the addresses that map to something else once the
                   real table is live. setup_free_pages_list hands them to the
                   runtime allocator (which only ever touches a page through
                   tmp_map) in a second pass, so nothing is leaked. Sorting
                   them here costs one comparison; it used to be a second walk
                   of the whole list, and the whole list is as long as the
                   machine has memory. */
                ulong* head = (address < exclude_limit) ? &excluded_pages_ : &free_pages_;

                *(ulong *)BuiltinPageTable::get_instance().phys_to_virt(address) = *head;
                *head = address;
            }
        }
        klog(PAGE_ALLOCATOR_LL, "get_free_pages: inner loop done, total %lu", total_pages_count_);
    }

    klog(PAGE_ALLOCATOR_LL, "get_free_pages done, highest_phy_addr_ 0x%lX TotalPages %lu",
        highest_phy_addr_, total_pages_count_);

    /* Say plainly how much of the machine's RAM the kernel actually took.
       The per-region traces above print the clamped end, so a box with more
       memory than the bootstrap map covers looked, in the log, exactly like
       a box with 4GiB in it. */
    ulong usable = mmap.get_usable_ram_bytes();
    ulong unreachable = mmap.get_usable_ram_bytes_above(builtin_map_limit);

    klog(0, "mm: %lu MiB usable RAM reported, %lu MiB reachable (%lu pages)",
        usable / consts::MB, (total_pages_count_ * consts::PAGE_SIZE) / consts::MB,
        total_pages_count_);

    if (unreachable != 0)
        klog(0, "mm: %lu MiB of it is above the bootstrap map at 0x%lX and is not used",
            unreachable / consts::MB, builtin_map_limit);

    return true;
}

ulong PageTable::get_free_page()
{
    if (free_pages_ == 0)
        return 0;

    ulong curr = free_pages_;
    ulong next = *(ulong *)BuiltinPageTable::get_instance().phys_to_virt(curr);
    free_pages_ = next;

    stdlib::mem_set((void *)BuiltinPageTable::get_instance().phys_to_virt(curr), 0, consts::PAGE_SIZE);
    return curr;
}

void PageTable::drain_early_free_list()
{
    /* The early list is threaded through the free pages themselves, so every
       step has to read one, and there are as many of them as the machine has
       RAM. Doing it with the general tmp_map_page/tmp_unmap_page pair costs two
       TLB invalidations, two lock round trips and two page_array_ lookups per
       page; one slot held across the whole walk costs one of each. Safe
       because this runs on the BSP before any AP is started. */
    const ulong virt_addr = tmp_map_start_;
    Pte* l1_entry = &tmp_map_l1_page_->entry[Pte::l1_index(virt_addr)];

    BUG_ON(l1_entry->present());

    /* Same eight-lines-on-the-biggest-machine budget as the scan. */
    const ulong progress_pages = (8 * consts::GB) / consts::PAGE_SIZE;
    ulong next_progress = ((free_pages_count_ / progress_pages) + 1) * progress_pages;

    while (free_pages_ != 0)
    {
        ulong phy_addr = free_pages_;

        l1_entry->value = 0;
        l1_entry->set_address(phy_addr);
        l1_entry->set_writable();
        l1_entry->set_no_execute(); /* a free page is data, like every tmp_map slot */
        l1_entry->set_present();
        hal::tlb_flush_page(virt_addr);

        free_pages_ = *(ulong *)virt_addr;

        /* Nothing is zeroed on the way to the list. A page goes onto the
           other list, as its last owner left it -- here, whatever the
           firmware, the loader or the last boot left -- and an allocation
           zeroes it, or the zeroing tasks do first (zero_free_page). Zeroing
           here was a write pass over every byte of RAM the machine has --
           two thirds of the time this took, on the one CPU the boot has. */
        Page* page = get_page(phy_addr);
        BUG_ON(!page);
        /* get_page's +1 was only for the lookup; a page on the free list must
           sit at refcount 1 */
        page->put();
        free_pages_list_.insert_head(&page->list_entry);
        free_pages_count_++;

        if (free_pages_count_ >= next_progress)
        {
            klog(0, "mm: %lu MiB onto the free list",
                (free_pages_count_ * consts::PAGE_SIZE) / consts::MB);
            next_progress += progress_pages;
        }
    }

    l1_entry->value = 0;
    hal::tlb_flush_page(virt_addr);
}

ulong PageTable::get_l1_page(ulong virt_addr)
{
    BUG_ON(virt_addr & (consts::PAGE_SIZE - 1));
    BUG_ON(!virt_addr);

    ulong l4_index = Pte::l4_index(virt_addr);
    ulong l3_index = Pte::l3_index(virt_addr);
    ulong l2_index = Pte::l2_index(virt_addr);

    if (!root_)
        return 0;

    PtePage* l4_page = (PtePage*)BuiltinPageTable::get_instance().phys_to_virt(root_);
    Pte *l4_entry = &l4_page->entry[l4_index];
    if (!l4_entry->present())
        return 0;

    PtePage* l3_page = (PtePage*)BuiltinPageTable::get_instance().phys_to_virt(l4_entry->address());
    Pte *l3_entry = &l3_page->entry[l3_index];
    if (!l3_entry->present() || l3_entry->huge())
        return 0;

    PtePage* l2_page = (PtePage*)BuiltinPageTable::get_instance().phys_to_virt(l3_entry->address());
    Pte *l2_entry = &l2_page->entry[l2_index];
    if (!l2_entry->present() || l2_entry->huge())
        return 0;

    return l2_entry->address();
}

ulong PageTable::virt_to_phys(ulong virt_addr)
{
    /* The walk below maps and unmaps shared tmp_map slots, and tmp_unmap_page
       only invalidates the local TLB. Disable preemption across the whole
       walk so a migration mid-walk cannot leave a stale mapping on the
       original CPU that a later slot reuse would mistranslate. */
    ulong flags = preempt_irq_save();
    ulong phys = virt_to_phys_locked(virt_addr);
    preempt_irq_restore(flags);
    return phys;
}

ulong PageTable::virt_to_phys_locked(ulong virt_addr)
{
    if (!root_)
        return 0;

    ulong l4_index = Pte::l4_index(virt_addr);
    ulong l3_index = Pte::l3_index(virt_addr);
    ulong l2_index = Pte::l2_index(virt_addr);
    ulong l1_index = Pte::l1_index(virt_addr);
    ulong offset  = virt_addr & (consts::PAGE_SIZE - 1);

    PtePage* l4_page = (PtePage*)tmp_map_page(root_);
    if (!l4_page)
        return 0;
    Pte l4_entry = l4_page->entry[l4_index];
    tmp_unmap_page((ulong)l4_page);
    if (!l4_entry.present())
        return 0;

    PtePage* l3_page = (PtePage*)tmp_map_page(l4_entry.address());
    if (!l3_page)
        return 0;
    Pte l3_entry = l3_page->entry[l3_index];
    tmp_unmap_page((ulong)l3_page);
    if (!l3_entry.present())
        return 0;

    /* 1GB block at the L3 level (the arm64 early device GiB); without this
       check the walk would treat the block's output address as an L2 table */
    if (l3_entry.huge())
        return l3_entry.address() + (virt_addr & (consts::GB - 1));

    PtePage* l2_page = (PtePage*)tmp_map_page(l3_entry.address());
    if (!l2_page)
        return 0;
    Pte l2_entry = l2_page->entry[l2_index];
    tmp_unmap_page((ulong)l2_page);
    if (!l2_entry.present())
        return 0;

    /* 2MB huge page at L2 level */
    if (l2_entry.huge())
    {
        ulong huge_offset = Pte::huge_offset(virt_addr);
        return l2_entry.address() + huge_offset;
    }

    PtePage* l1_page = (PtePage*)tmp_map_page(l2_entry.address());
    if (!l1_page)
        return 0;
    Pte l1_entry = l1_page->entry[l1_index];
    tmp_unmap_page((ulong)l1_page);
    if (!l1_entry.present())
        return 0;

    return l1_entry.address() + offset;
}

bool PageTable::setup_page(ulong virt_addr, ulong phy_addr, bool executable)
{
    BUG_ON(virt_addr & (consts::PAGE_SIZE - 1));
    BUG_ON(phy_addr & (consts::PAGE_SIZE - 1));

    ulong l4_index = Pte::l4_index(virt_addr);
    ulong l3_index = Pte::l3_index(virt_addr);
    ulong l2_index = Pte::l2_index(virt_addr);
    ulong l1_index = Pte::l1_index(virt_addr);

    if (root_ == 0)
    {
        root_ = get_free_page();
        if (!root_)
            return false;
        klog(0, "Root 0x%lX", root_);
    }

    PtePage* l4_page = (PtePage*)BuiltinPageTable::get_instance().phys_to_virt(root_);
    Pte *l4_entry = &l4_page->entry[l4_index];
    if (!l4_entry->present()) {
        ulong addr = get_free_page();
        if (addr == 0)
            return false;

        l4_entry->set_address(addr);
        l4_entry->set_writable();
        l4_entry->set_present();
    }

    PtePage* l3_page = (PtePage*)BuiltinPageTable::get_instance().phys_to_virt(l4_entry->address());
    Pte *l3_entry = &l3_page->entry[l3_index];
    if (!l3_entry->present()) {
        ulong addr = get_free_page();
        if (addr == 0)
            return false;

        l3_entry->set_address(addr);
        l3_entry->set_writable();
        l3_entry->set_present();
    }

    PtePage* l2_page = (PtePage*)BuiltinPageTable::get_instance().phys_to_virt(l3_entry->address());
    Pte *l2_entry = &l2_page->entry[l2_index];
    if (!l2_entry->present()) {
        ulong addr = get_free_page();
        if (addr == 0)
            return false;

        l2_entry->set_address(addr);
        l2_entry->set_writable();
        l2_entry->set_present();
    }

    PtePage* l1_page = (PtePage*)BuiltinPageTable::get_instance().phys_to_virt(l2_entry->address());
    Pte *l1_entry = &l1_page->entry[l1_index];

    klog(5, "va 0x%lX pha 0x%lX l4 %lu(0x%p) l3 %lu(0x%p) l2 %lu(0x%p) l1 %lu(0x%p)",
        virt_addr, phy_addr, l4_index, l4_entry, l3_index, l3_entry, l2_index, l2_entry, l1_index, l1_entry);

    if (l1_entry->present())
        return false;

    if (phy_addr)
    {
        l1_entry->set_address(phy_addr);
        l1_entry->set_writable();
        if (!executable)
            l1_entry->set_no_execute();
        l1_entry->set_present();
    } else {
        l1_entry->clear();
    }
    hal::tlb_flush_page(virt_addr);
    return true;
}

bool PageTable::setup_huge_page(ulong virt_addr, ulong phy_addr)
{
    BUG_ON(virt_addr & (HUGE_PAGE_SIZE - 1));
    BUG_ON(phy_addr & (HUGE_PAGE_SIZE - 1));
    BUG_ON(root_ == 0);

    auto& bpt = BuiltinPageTable::get_instance();

    PtePage* l4_page = (PtePage*)bpt.phys_to_virt(root_);
    Pte* l4_entry = &l4_page->entry[Pte::l4_index(virt_addr)];
    if (!l4_entry->present())
    {
        ulong addr = get_free_page();
        if (addr == 0)
            return false;

        l4_entry->set_address(addr);
        l4_entry->set_writable();
        l4_entry->set_present();
    }

    PtePage* l3_page = (PtePage*)bpt.phys_to_virt(l4_entry->address());
    Pte* l3_entry = &l3_page->entry[Pte::l3_index(virt_addr)];
    if (!l3_entry->present())
    {
        ulong addr = get_free_page();
        if (addr == 0)
            return false;

        l3_entry->set_address(addr);
        l3_entry->set_writable();
        l3_entry->set_present();
    }

    /* A 1GiB block here instead of a table would swallow everything else
       living in this GiB of kernel VA. */
    if (l3_entry->huge())
        return false;

    PtePage* l2_page = (PtePage*)bpt.phys_to_virt(l3_entry->address());
    Pte* l2_entry = &l2_page->entry[Pte::l2_index(virt_addr)];
    if (l2_entry->present())
        return false;

    /* The permissions setup_page's 4KiB leaf gets for data, no more and no
       less: page_array_ is written and read, never executed. */
    l2_entry->set_address(phy_addr);
    l2_entry->set_writable();
    l2_entry->set_no_execute();
    l2_entry->set_huge();
    l2_entry->set_present();
    hal::tlb_flush_page(virt_addr);
    return true;
}

ulong PageTable::reserve_page_array(ulong bytes, ulong lower_bound)
{
    auto& mmap = MemoryMap::get_instance();
    auto& bpt = BuiltinPageTable::get_instance();

    /* It has to be writable through the bootstrap map: Setup fills page_array_
       in through phys_to_virt, while the real table is not live yet. */
    const ulong map_limit = bpt.get_mapped_limit();

    const ulong kernel_start = bpt.virt_to_phys(mmap.get_kernel_start());
    const ulong kernel_end = bpt.virt_to_phys(mmap.get_kernel_end());

    for (size_t i = 0; i < mmap.get_region_count(); i++)
    {
        ulong addr, len, type;
        if (!mmap.get_region(i, addr, len, type))
            break;

        if (type != MemoryMap::USABLE_RAM_TYPE)
            continue;

        ulong start = stdlib::round_up((addr < lower_bound) ? lower_bound : addr,
            HUGE_PAGE_SIZE);

        ulong end = addr + len;
        if (end > map_limit)
            end = map_limit;

        if (start >= end || (end - start) < bytes)
            continue;

        for (ulong cand = start; (cand + bytes) <= end; cand += HUGE_PAGE_SIZE)
        {
            /* The kernel image is plain usable RAM as far as the firmware is
               concerned; nobody marks it reserved. */
            if (cand < kernel_end && (cand + bytes) > kernel_start)
                continue;

            if (mmap.is_reserved(cand, bytes))
                continue;

            return cand;
        }
    }

    return 0;
}

bool PageTable::change_range_protection(ulong virt_addr, ulong size_bytes,
    bool writable, bool executable, bool exact)
{
    stdlib::AutoLock lock(lock_);

    if (root_ == 0)
        return false;

    BUG_ON(virt_addr & (consts::PAGE_SIZE - 1));

    ulong end = virt_addr + stdlib::round_up(size_bytes, consts::PAGE_SIZE);

    /* Runs after the real page table is active, so the page-table pages are
       reached through the tmp_map window (the builtin linear map is gone),
       exactly like map_page/unmap_page. */
    for (ulong va = virt_addr; va < end; va += consts::PAGE_SIZE)
    {
        /* A window with no slot left is a failure the caller is told of:
           each of the four used to be dereferenced as it came back */
        PtePage* l4 = (PtePage*)tmp_map_page(root_);
        if (l4 == nullptr)
            return false;
        Pte* l4e = &l4->entry[Pte::l4_index(va)];
        if (!l4e->present()) { tmp_unmap_page((ulong)l4); return false; }
        ulong l3phys = l4e->address();
        tmp_unmap_page((ulong)l4);

        PtePage* l3 = (PtePage*)tmp_map_page(l3phys);
        if (l3 == nullptr)
            return false;
        Pte* l3e = &l3->entry[Pte::l3_index(va)];
        if (!l3e->present() || l3e->huge())
        {
            tmp_unmap_page((ulong)l3); /* kernel image is 4KiB-mapped */
            return false;
        }
        ulong l2phys = l3e->address();
        tmp_unmap_page((ulong)l3);

        PtePage* l2 = (PtePage*)tmp_map_page(l2phys);
        if (l2 == nullptr)
            return false;
        Pte* l2e = &l2->entry[Pte::l2_index(va)];
        if (!l2e->present() || l2e->huge())
        {
            tmp_unmap_page((ulong)l2); /* kernel image is 4KiB-mapped */
            return false;
        }
        ulong l1phys = l2e->address();
        tmp_unmap_page((ulong)l2);

        PtePage* l1 = (PtePage*)tmp_map_page(l1phys);
        if (l1 == nullptr)
            return false;
        Pte* l1e = &l1->entry[Pte::l1_index(va)];
        if (!l1e->present()) { tmp_unmap_page((ulong)l1); return false; }

        if (!writable)
            l1e->set_read_only();
        else if (exact)
            l1e->set_writable();
        if (!executable)
            l1e->set_no_execute();
        else if (exact)
            l1e->clear_no_execute();
        tmp_unmap_page((ulong)l1);

        invalidate_local_tlb_address(va);
    }

    return true;
}

bool PageTable::protect_range(ulong virt_addr, ulong size_bytes, bool writable,
    bool executable)
{
    return change_range_protection(virt_addr, size_bytes, writable, executable, false);
}

bool PageTable::set_range_protection(ulong virt_addr, ulong size_bytes, bool writable,
    bool executable)
{
    return change_range_protection(virt_addr, size_bytes, writable, executable, true);
}

Page* PageTable::get_page(ulong phy_addr)
{
    BUG_ON(phy_addr & (consts::PAGE_SIZE - 1));
    ulong index = phy_addr / consts::PAGE_SIZE;

    BUG_ON(index >= page_array_count_);
    Page* page = &page_array_[index];
    BUG_ON(page->get_phy_address() != phy_addr);
    page->get();
    return page;
}

bool PageTable::setup()
{
    klog(0, "PageTable setup");

    auto& mmap = MemoryMap::get_instance();
    auto& bpt = BuiltinPageTable::get_instance();

    /* page_array_ has to be sized and placed before the free list is built --
       its home is carved out of the map so the list never sees those pages --
       so the top of usable memory is computed here rather than picked up
       from get_free_pages. Same clamp get_free_pages applies per region: the
       list can only hold what the bootstrap map reaches. */
    highest_phy_addr_ = stdlib::round_up(mmap.get_usable_ram_end(), consts::PAGE_SIZE);
    if (highest_phy_addr_ > bpt.get_mapped_limit())
        highest_phy_addr_ = bpt.get_mapped_limit();

    tmp_map_l1_page_ = (PtePage *)mmap.get_kernel_end();
    tmp_map_start_ = stdlib::round_up(mmap.get_kernel_end() + consts::PAGE_SIZE, HUGE_PAGE_SIZE);
    page_array_ = (Page*)(tmp_map_start_ + TMP_MAP_PAGE_COUNT * consts::PAGE_SIZE);
    BUG_ON((ulong)page_array_ & (HUGE_PAGE_SIZE - 1));

    ulong page_array_count = highest_phy_addr_ / consts::PAGE_SIZE + 1;
    ulong page_array_bytes = stdlib::round_up(page_array_count * sizeof(Page), HUGE_PAGE_SIZE);
    ulong page_array_limit = (ulong)page_array_ + page_array_bytes;

    /* Above the VA window's own physical shadow, so the carve-out is nowhere
       near the pages ExcludeFreePages parks. */
    page_array_phys_ = reserve_page_array(page_array_bytes,
        stdlib::round_up(bpt.virt_to_phys(page_array_limit), HUGE_PAGE_SIZE));
    if (page_array_phys_ == 0)
    {
        klog(0, "mm: no %lu MiB contiguous window for page_array_",
            page_array_bytes / consts::MB);
        return false;
    }

    /* Reserving it in the map is the whole mechanism: get_free_pages already
       skips any page a reserved region covers. */
    if (!mmap.add_region(page_array_phys_, page_array_bytes, MemoryMap::RESERVED_TYPE))
    {
        klog(0, "mm: no room in the memory map to reserve page_array_");
        return false;
    }

    klog(0, "mm: page_array_ %lu MiB at phys 0x%lX, %lu huge pages",
        page_array_bytes / consts::MB, page_array_phys_, page_array_bytes / HUGE_PAGE_SIZE);

    if (!get_free_pages(bpt.virt_to_phys(page_array_limit)))
        return false;

    for (ulong address = mmap.get_kernel_start(); address < mmap.get_kernel_end(); address+= consts::PAGE_SIZE)
    {
        /* The one executable mapping the table is built with: the kernel
           runs out of it from the root switch until protect_range splits
           it into text RX / rodata RO+NX / data RW+NX. */
        if (!setup_page(address, BuiltinPageTable::get_instance().virt_to_phys(address), true))
        {
            klog(0, "can't setup page");
            return false;
        }
    }

    klog(0, "tmp_map_start_ 0x%lX", tmp_map_start_);
    for (size_t i = 0; i < TMP_MAP_PAGE_COUNT; i++)
    {
        if (!setup_page(tmp_map_start_ + i * consts::PAGE_SIZE, 0, false))
            return false;
    }
    for (size_t i = 0; i < stdlib::array_size(tmp_map_page_array_); i++)
        tmp_map_page_array_[i] = nullptr;

    /* One L1 table for the whole window, the CPUs' frame slots included:
       tmp_map_l1_page_ is what every slot's entry is written through. */
    auto tmp_map_l1_page_phy_addr = get_l1_page(tmp_map_start_);
    for (size_t i = 0; i < TMP_MAP_PAGE_COUNT; i++)
        BUG_ON(tmp_map_l1_page_phy_addr != get_l1_page(tmp_map_start_ + i * consts::PAGE_SIZE));

    if (!setup_page((ulong)tmp_map_l1_page_, tmp_map_l1_page_phy_addr, false))
        return false;

    if (!setup_page(0, 0, false))
        return false;

    klog(0, "page_array_ setup, highestPhyAddr 0x%lX", highest_phy_addr_);

    klog(0, "page_array_ setup, page_array 0x%p page_array_limit 0x%lX", page_array_, page_array_limit);

    for (ulong offset = 0; offset < page_array_bytes; offset += HUGE_PAGE_SIZE)
    {
        if (!setup_huge_page((ulong)page_array_ + offset, page_array_phys_ + offset))
        {
            klog(0, "mm: can't map page_array_ at 0x%lX", (ulong)page_array_ + offset);
            return false;
        }
    }

    /* Filled through the bootstrap map rather than through page_array_'s own
       virtual address: the real table is not live yet, and the backing is
       one contiguous physical run, so this is a straight linear write. */
    Page* pages = (Page*)bpt.phys_to_virt(page_array_phys_);
    for (ulong i = 0; i < page_array_count; i++)
    {
        pages[i].init(i * consts::PAGE_SIZE);

        /* ListEntry::init stores `this`, and `this` here is the descriptor's
           address in the bootstrap map, not the one page_array_ gets in the
           real table. The drain repairs every descriptor it inserts, but one
           that never reaches the free list would keep a self-pointer that is
           not equal to its own address -- and "self-pointing means not on the
           free list" is exactly the test alloc_contiguous_pages uses to decide
           whether it may take a page. It would read a reserved page, or a
           page below the start of RAM, as free, hand out the run and zero
           it. Point them at the address the descriptor will answer to. */
        pages[i].list_entry.flink = &page_array_[i].list_entry;
        pages[i].list_entry.blink = &page_array_[i].list_entry;
    }

    page_array_count_ = page_array_count;

    klog(0, "page_array_ setup done, count %lu", page_array_count_);
    return true;
}

bool PageTable::setup_free_pages_list()
{
    free_pages_count_ = 0;

    /* Pass 1 drains the early free list; pass 2 drains the pages Setup
       parked below the page_array_ limit (plain usable RAM, safe now that all
       page access goes through tmp_map). */
    for (ulong pass = 0; pass < 2; pass++)
    {
        drain_early_free_list();

        free_pages_ = excluded_pages_;
        excluded_pages_ = 0;
    }

    /* The lowest from here: before, the count only grew from nothing */
    free_pages_lowest_ = free_pages_count_;
    klog(0, "free_pages_count_ %lu", free_pages_count_);
    return true;
}

void PageTable::check_free_list(stdlib::Printer& printer)
{
    auto& mmap = MemoryMap::get_instance();
    auto& bpt = BuiltinPageTable::get_instance();

    const ulong kernel_phys_start = bpt.virt_to_phys(mmap.get_kernel_start());
    const ulong kernel_phys_end = bpt.virt_to_phys(mmap.get_kernel_end());

    ulong on_list = 0;
    ulong marked = 0;
    ulong in_reserved = 0;
    ulong in_kernel = 0;
    ulong outside_ram = 0;
    ulong wrong_phys = 0;

    /* Reserved-ness changes only at region boundaries, so carry a window
       forward instead of asking about every page. */
    ulong window_end = 0;
    bool window_reserved = false;

    for (ulong pfn = 0; pfn < page_array_count_; pfn++)
    {
        ulong phy_addr = pfn * consts::PAGE_SIZE;

        if (phy_addr >= window_end)
        {
            ulong reserved_end = mmap.get_reserved_end(phy_addr);
            if (reserved_end != 0)
            {
                window_reserved = true;
                window_end = reserved_end;
            }
            else
            {
                window_reserved = false;
                window_end = mmap.get_next_reserved_start(phy_addr,
                    page_array_count_ * consts::PAGE_SIZE);
            }
        }

        Page* page = &page_array_[pfn];
        if (page->get_phy_address() != phy_addr)
        {
            wrong_phys++;
            continue;
        }

        /* Zeroed, or on its way to the zeroed list (zero_free_page) */
        if (page->is_zeroed())
            marked++;

        /* A self-pointing ListEntry means the page is not on the free list,
           the same test alloc_contiguous_pages uses. */
        if (page->list_entry.flink == &page->list_entry)
            continue;

        on_list++;

        if (window_reserved)
            in_reserved++;
        if (phy_addr >= kernel_phys_start && phy_addr < kernel_phys_end)
            in_kernel++;
        if (!mmap.is_usable_ram(phy_addr))
            outside_ram++;
    }

    printer.printf("pages: %lu descriptors, %lu on the free lists (counter says %lu)\n",
        page_array_count_, on_list, free_pages_count_);
    printer.printf("zeroed or being zeroed: %lu (counter says %lu on the zeroed list)\n",
        marked, zeroed_pages_count_);
    printer.printf("free pages inside a reserved region: %lu\n", in_reserved);
    printer.printf("free pages inside the kernel image:  %lu\n", in_kernel);
    printer.printf("free pages outside usable RAM:       %lu\n", outside_ram);
    printer.printf("descriptors with a wrong phys addr:  %lu\n", wrong_phys);

    bool ok = (in_reserved == 0) && (in_kernel == 0) && (outside_ram == 0) &&
        (wrong_phys == 0);
    printer.printf("memcheck: %s (live snapshot, not locked)\n",
        ok ? "ok" : "FAILED");
}

/* The page is zeroed off the lock. It was zeroed under it, and every CPU
   that allocated waited for every other's zeroing -- 4 KiB of stores on
   hardware, and seconds under TCG once the page has held a guest's code:
   QEMU throws its translations of a page away as the page is written, and
   rebuilding a guest that had run for half a minute held this lock for
   more than the watchdog's ten seconds, with every other CPU idle. */
Page* PageTable::alloc_page()
{
    Page* page;
    bool zeroed = false;
    {
        stdlib::AutoLock lock(lock_);
        page = take_free_page_no_lock(false, zeroed);
    }
    if (page)
        zero_unless_zeroed(page, zeroed);
    return page;
}

Page* PageTable::alloc_page_keeping(ulong keep)
{
    Page* page = nullptr;
    bool zeroed = false;
    {
        stdlib::AutoLock lock(lock_);
        if (free_pages_count_ > keep)
            page = take_free_page_no_lock(true, zeroed);
    }
    if (page)
        zero_unless_zeroed(page, zeroed);
    return page;
}

Page* PageTable::alloc_page_no_lock()
{
    bool zeroed = false;
    Page* page = take_free_page_no_lock(false, zeroed);
    if (page)
        zero_unless_zeroed(page, zeroed);
    return page;
}

Page* PageTable::take_free_page_no_lock(bool zeroed_first, bool& zeroed)
{
    stdlib::ListEntry* list = zeroed_first ? &zeroed_pages_list_ : &free_pages_list_;
    if (list->is_empty())
        list = zeroed_first ? &free_pages_list_ : &zeroed_pages_list_;
    if (list->is_empty())
        return nullptr;

    Page* page = CONTAINING_RECORD(list->remove_head(), Page, list_entry);
    page->list_entry.init(); /* Self-pointing = not on any list */
    check_unreferenced(page);
    zeroed = (list == &zeroed_pages_list_);
    /* The mark and the list are changed together, under Lock: a page on the
       zeroed list without it, or on the other with it, is the two come
       apart -- a page handed out as zero that is not */
    BUG_ON(page->is_zeroed() != zeroed);
    page->set_zeroed(false);
    BUG_ON(free_pages_count_ == 0);
    free_pages_count_--;
    free_pages_lowest_ = stdlib::min(free_pages_lowest_, free_pages_count_);
    if (zeroed)
    {
        BUG_ON(zeroed_pages_count_ == 0);
        zeroed_pages_count_--;
    }
    return page;
}

/* A page taken off a free list has its descriptor's own reference and no
   other: every mapping holds one more (map_range_locked) and gives it back
   as it goes (unmap_range_locked, under Lock with the free), so one with
   more is a page freed while something still maps it -- about to be handed
   to a second owner while the first writes it, each over the other's data
   with nothing to say so. Under Lock. */
void PageTable::check_unreferenced(Page* page)
{
    const long refs = page->ref_count.get();
    if (refs != 1)
        PANIC("mm: free page 0x%lX has %ld references, not 1: freed while still mapped", page->get_phy_address(),
            refs);
}

void PageTable::unlist_free_page_no_lock(Page* page)
{
    BUG_ON(page->list_entry.flink == &page->list_entry);
    page->list_entry.remove_init();
    BUG_ON(free_pages_count_ == 0);
    free_pages_count_--;
    free_pages_lowest_ = stdlib::min(free_pages_lowest_, free_pages_count_);
    if (page->is_zeroed())
    {
        BUG_ON(zeroed_pages_count_ == 0);
        zeroed_pages_count_--;
        page->set_zeroed(false);
    }
}

void PageTable::zero_unless_zeroed(Page* page, bool zeroed)
{
    if (!zeroed)
    {
        zero_page(page);
        return;
    }
    if (!zero_check_)
        return;

    /* Mapped as zero_page maps it, and for the same reason */
    Task* task = preempt_disable_task();
    ulong va = tmp_map_page(page->get_phy_address());
    if (va == 0)
        PANIC("zero_unless_zeroed: no tmp_map slot for page 0x%lX: the window's %lu shared slots are all held",
            page->get_phy_address(), (ulong)TMP_MAP_SHARED_COUNT);
    check_zeroed(va, page->get_phy_address());
    tmp_unmap_page(va);
    preempt_enable_task(task);
}

void PageTable::check_zeroed(ulong va, ulong phy_addr)
{
    const ulong* words = reinterpret_cast<const ulong*>(va);
    for (ulong i = 0; i < consts::PAGE_SIZE / sizeof(ulong); i++)
    {
        if (words[i] != 0)
            PANIC("mm: free page 0x%lX written after it was zeroed: 0x%lX at offset 0x%lX -- a use after free",
                phy_addr, words[i], i * sizeof(ulong));
    }
}

void PageTable::set_zero_check(bool on)
{
    zero_check_ = on;
}

bool PageTable::is_zero_check()
{
    return zero_check_;
}

bool PageTable::zero_free_page(ulong keep)
{
    ulong flags = preempt_irq_save();
    Page* page = nullptr;
    {
        stdlib::AutoLock lock(lock_);
        if (free_pages_count_ > keep && !free_pages_list_.is_empty())
        {
            page = CONTAINING_RECORD(free_pages_list_.remove_tail(), Page, list_entry);
            page->list_entry.init(); /* on no list: no run takes it */
            BUG_ON(page->is_zeroed());
            /* Marked while on its way: a free of it is a page freed twice,
               and panics (free_page_no_lock). Still counted free. */
            page->set_zeroed(true);
        }
    }
    if (page == nullptr)
    {
        preempt_irq_restore(flags);
        return false;
    }

    const ulong va = map_frame_slot(page->get_phy_address());
    stdlib::mem_set(reinterpret_cast<void*>(va), 0, consts::PAGE_SIZE);
    unmap_frame_slot(va);

    {
        stdlib::AutoLock lock(lock_);
        BUG_ON(!page->is_zeroed() || page->list_entry.flink != &page->list_entry);
        zeroed_pages_list_.insert_head(&page->list_entry);
        zeroed_pages_count_++;
    }
    preempt_irq_restore(flags);
    return true;
}

ulong PageTable::get_zeroed_pages_count()
{
    stdlib::AutoLock lock(lock_);
    return zeroed_pages_count_;
}

/* Preemption is off from the map to the unmap, as a spinlock kept it when
   this ran under Lock: the unmap invalidates only this CPU's TLB, so a task
   moved to another CPU half way would leave the slot's translation stale
   behind it -- and the next page mapped at that slot there would be written
   through it. */
void PageTable::zero_page(Page* page)
{
    Task* task = preempt_disable_task();
    ulong va = tmp_map_page(page->get_phy_address());
    if (va == 0)
        PANIC("zero_page: no tmp_map slot for page 0x%lX: the window's %lu shared slots are all held",
            page->get_phy_address(), (ulong)TMP_MAP_SHARED_COUNT);
    stdlib::mem_set((void*)va, 0, consts::PAGE_SIZE);
    tmp_unmap_page(va);
    preempt_enable_task(task);
}

/* Walk the free lists and for each free page, check whether the next
   count-1 descriptors of page_array_ are free too and physically
   contiguous. A page is on a free list if its ListEntry is NOT self-pointing
   (a page taken off one is re-initialised to point at itself). A run starts
   on one of the two lists and may go on across both: its first page is
   looked for on each, and the rest only have to be free. */
Page* PageTable::take_run_no_lock(ulong count, bool* zeroed, ulong& examined)
{
    stdlib::ListEntry* const lists[] = { &free_pages_list_, &zeroed_pages_list_ };
    for (stdlib::ListEntry* list : lists)
    {
        for (auto* entry = list->flink; entry != list; entry = entry->flink)
        {
            examined++;
            Page* first = CONTAINING_RECORD(entry, Page, list_entry);
            const ulong base_phy_addr = first->get_phy_address();
            const ulong base_index = (ulong)(first - page_array_);
            if (base_index + count > page_array_count_)
                continue;

            bool ok = true;
            for (ulong j = 0; j < count && ok; j++)
            {
                Page* p = &page_array_[base_index + j];
                /* At its own address, and not self-pointing: a self-pointing
                   ListEntry is a page on neither list -- handed out, or being
                   zeroed (zero_free_page) */
                ok = p->get_phy_address() == base_phy_addr + j * consts::PAGE_SIZE &&
                    p->list_entry.flink != &p->list_entry;
            }
            if (!ok)
                continue;

            /* Found: every page of it off its list, its mark said first */
            for (ulong j = 0; j < count; j++)
            {
                Page* p = &page_array_[base_index + j];
                zeroed[j] = p->is_zeroed();
                unlist_free_page_no_lock(p);
                check_unreferenced(p);
            }
            return first;
        }
    }
    return nullptr;
}

/* The run is zeroed off the lock, a page at a time, as alloc_page zeroes a
   page: once off the lists it is the caller's alone. It was zeroed under
   Lock -- a run of 128 pages half a megabyte of stores with interrupts off,
   every other CPU's allocation waiting for it, and under TCG far longer for
   pages that had held a guest's code. */
Page* PageTable::alloc_contiguous_pages(ulong count)
{
    if (count == 0 || count > MAX_CONTIGUOUS_PAGES)
        return nullptr;

    /* The walk is unbounded by design: capping it would turn a slow
       allocation into a spurious out-of-memory, and plain operator new
       panics on that. What it must not do is be invisible -- it runs with
       Lock held and interrupts off, so a long one is a stall every other
       CPU feels. Said once it got there, after the lock. */
    static const ulong LONG_SEARCH_ENTRIES = 1024;
    ulong examined = 0;
    /* Which pages of the run were known zero, as they came off their lists */
    bool zeroed[MAX_CONTIGUOUS_PAGES] = {};
    Page* run = nullptr;
    {
        stdlib::AutoLock lock(lock_);
        run = take_run_no_lock(count, zeroed, examined);
    }

    if (run == nullptr)
    {
        /* Said only of a walk: with nothing free there was none, as before */
        if (examined != 0)
            klog(0, "alloc_contiguous_pages: no run of %lu pages in %lu entries", count, examined);
        return nullptr;
    }
    if (examined > LONG_SEARCH_ENTRIES)
        klog(0, "alloc_contiguous_pages: %lu pages found after %lu entries", count, examined);

    for (ulong j = 0; j < count; j++)
        zero_unless_zeroed(&run[j], zeroed[j]);
    return run;
}

void PageTable::free_page_no_lock(Page* page)
{
    /* A page handed out has no mark (take_free_page_no_lock,
       unlist_free_page_no_lock): one with it is free already -- zeroed, or
       being zeroed -- and freed a second time. */
    if (page->is_zeroed())
        PANIC("free_page: page 0x%lX is free already: freed twice", page->get_phy_address());
    /* Nor is one handed out on a list: its entry points at itself. One on a
       list is free already, not zeroed yet, and freed a second time -- put
       on the list again, its old neighbours would still point at it, and a
       take of one of them would hand the page out once more while its
       first owner still has it: two owners of one frame, each writing over
       the other's page, and nothing to say so. */
    if (page->list_entry.flink != &page->list_entry)
        PANIC("free_page: page 0x%lX is on a free list already: freed twice", page->get_phy_address());
    /* Back as its owner left it: onto the other list, whose hot end the
       allocations take from */
    free_pages_count_++;
    free_pages_list_.insert_head(&page->list_entry);
}

void PageTable::free_page(Page* page)
{
    stdlib::AutoLock lock(lock_);

    free_page_no_lock(page);
}

bool PageTable::is_frame_address(ulong phy_addr)
{
    if (phy_addr & (consts::PAGE_SIZE - 1))
        return false;

    /* page_array_ and its count are set once in setup() and never change. */
    ulong index = phy_addr / consts::PAGE_SIZE;
    if (page_array_ == nullptr || index >= page_array_count_)
        return false;

    return page_array_[index].get_phy_address() == phy_addr;
}

void PageTable::free_frame(ulong phy_addr)
{
    BUG_ON(!is_frame_address(phy_addr));
    /* A page of the huge pool goes back with its block, into the pool: on
       the free list too it would be handed out twice. */
    ulong block = 0;
    if (huge_pool_block_of(phy_addr, block))
        PANIC("free_frame: page 0x%lX is in block %lu of the huge pool", phy_addr, block);

    stdlib::AutoLock lock(lock_);

    Page* page = &page_array_[phy_addr / consts::PAGE_SIZE];
    /* A page handed out has a self-pointing entry (alloc_page_no_lock); one on
       the free list points into it. Inserting the second kind again would
       make the list a loop. */
    BUG_ON(page->list_entry.flink != &page->list_entry);
    free_page_no_lock(page);
}

ulong PageTable::get_kernel_reserve_pages()
{
    /* total_pages_count_ is set once, by Setup. */
    return stdlib::max(KERNEL_RESERVE_MIN_BYTES / consts::PAGE_SIZE,
        total_pages_count_ / KERNEL_RESERVE_FRACTION);
}

bool PageTable::huge_pool_test(const ulong* bits, ulong block)
{
    return ((bits[block / HUGE_POOL_WORD_BITS] >> (block % HUGE_POOL_WORD_BITS)) & 1) != 0;
}

void PageTable::huge_pool_set(ulong* bits, ulong block, bool value)
{
    const ulong mask = 1UL << (block % HUGE_POOL_WORD_BITS);
    ulong& word = bits[block / HUGE_POOL_WORD_BITS];
    word = value ? (word | mask) : (word & ~mask);
}

bool PageTable::huge_pool_block_of(ulong phy_addr, ulong& block)
{
    if (phy_addr >= HUGE_POOL_MAX_BYTES)
        return false;
    block = phy_addr / HUGE_FRAME_SIZE;
    /* Written only before there is another CPU: read without the lock. */
    return huge_pool_test(huge_pool_in_, block);
}

/* Whether every page of the block is on the free list: a descriptor at its
   own address for each, and none self-pointing. Under Lock. */
bool PageTable::huge_block_free(ulong block)
{
    const ulong first = block * HUGE_FRAME_PAGES;
    if (first >= page_array_count_ || page_array_count_ - first < HUGE_FRAME_PAGES)
        return false;
    for (ulong i = first; i < first + HUGE_FRAME_PAGES; i++)
    {
        Page* page = &page_array_[i];
        if (page->get_phy_address() != i * consts::PAGE_SIZE || page->list_entry.flink == &page->list_entry)
            return false;
    }
    return true;
}

ulong PageTable::reserve_huge_pool(ulong bytes)
{
    return reserve_huge_pool_keeping(bytes, 2 * get_kernel_reserve_pages());
}

ulong PageTable::reserve_huge_pool_keeping(ulong bytes, ulong keep_pages)
{
    const ulong want = bytes / HUGE_FRAME_SIZE;
    if (want == 0)
        return 0;

    stdlib::AutoLock lock(lock_);
    stdlib::AutoLock pool_lock(huge_pool_lock_);

    /* Once: a second call would find the bitmaps another CPU may be
       reading without the lock. */
    BUG_ON(huge_pool_count_ != 0);
    const ulong blocks = stdlib::min(page_array_count_ / HUGE_FRAME_PAGES, HUGE_POOL_MAX_BLOCKS);
    for (ulong block = blocks; block-- > 0 && huge_pool_count_ < want;)
    {
        if (free_pages_count_ < keep_pages || free_pages_count_ - keep_pages < HUGE_FRAME_PAGES)
            break;
        if (!huge_block_free(block))
            continue;
        /* Off whichever list each page is on, its mark and the counts with
           it: the block is not known zero, whatever its pages were */
        for (ulong i = 0; i < HUGE_FRAME_PAGES; i++)
            unlist_free_page_no_lock(&page_array_[block * HUGE_FRAME_PAGES + i]);
        huge_pool_set(huge_pool_in_, block, true);
        huge_pool_set(huge_pool_free_bits_, block, true);
        huge_pool_count_++;
    }
    huge_pool_free_count_ = huge_pool_count_;
    /* Said either way: a pool cut short is guests that will not start. */
    klog(0, "mm: hugepages: %lu 2MiB blocks reserved (%lu MiB) of %lu asked, %lu MiB left free",
        huge_pool_count_, huge_pool_count_ * HUGE_FRAME_SIZE / consts::MB, want,
        free_pages_count_ * consts::PAGE_SIZE / consts::MB);
    return huge_pool_count_;
}

bool PageTable::huge_pool_find(const ulong* bits, const ulong* minus, ulong& block)
{
    for (ulong n = 0; n < HUGE_POOL_WORDS; n++)
    {
        const ulong w = (huge_pool_hint_ + n) % HUGE_POOL_WORDS;
        const ulong word = bits[w] & ~(minus != nullptr ? minus[w] : 0UL);
        if (word == 0)
            continue;
        /* word is not 0: its lowest set bit is a block */
        block = w * HUGE_POOL_WORD_BITS + static_cast<ulong>(__builtin_ctzl(word));
        return true;
    }
    return false;
}

void PageTable::zero_huge_page(ulong phy_addr)
{
    ulong flags = preempt_irq_save();
    ulong va = map_frame_slot(phy_addr);
    stdlib::mem_set(reinterpret_cast<void*>(va), 0, consts::PAGE_SIZE);
    unmap_frame_slot(va);
    preempt_irq_restore(flags);
}

void PageTable::check_huge_page_zeroed(ulong phy_addr)
{
    ulong flags = preempt_irq_save();
    ulong va = map_frame_slot(phy_addr);
    check_zeroed(va, phy_addr);
    unmap_frame_slot(va);
    preempt_irq_restore(flags);
}

ulong PageTable::alloc_huge_frame()
{
    ulong block = 0;
    /* How many of its pages, from the first, are known zero: all of a block
       known zero, what the zeroing had done of the block it was part way
       through, none of any other */
    ulong done = 0;
    {
        stdlib::AutoLock lock(huge_pool_lock_);
        if (huge_pool_free_count_ == 0)
            return 0;
        /* A block known zero first, handed out as it is. Else the block the
           zeroing is part way through: its first pages are zeros already --
           zeroed under this lock, the block free since -- and only the rest
           is zeroed below, so that while a guest takes its memory, a zeroing
           task on an idle CPU and the allocation share each block's work
           rather than do it twice; the zeroing goes on with another. Else
           any free one. A count that says there is one and no bit that says
           which is the two come apart, and a block handed out of that is
           anyone's. */
        if (huge_pool_zeroed_count_ != 0)
        {
            if (!huge_pool_find(huge_pool_zeroed_bits_, nullptr, block))
                PANIC("alloc_huge_frame: %lu blocks known zero by the count, none by the bitmap", huge_pool_zeroed_count_);
            /* Never the zeroing's: it takes no block known zero */
            BUG_ON(block == huge_zero_block_);
            huge_pool_set(huge_pool_zeroed_bits_, block, false);
            huge_pool_zeroed_count_--;
            done = HUGE_FRAME_PAGES;
        }
        else if (huge_zero_block_ != NO_HUGE_BLOCK)
        {
            /* Started on and not finished: a page done, and not all 512 */
            BUG_ON(huge_zero_next_ == 0 || huge_zero_next_ >= HUGE_FRAME_PAGES);
            block = huge_zero_block_;
            done = huge_zero_next_;
            huge_zero_block_ = NO_HUGE_BLOCK;
        }
        else if (!huge_pool_find(huge_pool_free_bits_, nullptr, block))
            PANIC("alloc_huge_frame: %lu blocks free by the count, none by the bitmap", huge_pool_free_count_);
        BUG_ON(!huge_pool_test(huge_pool_in_, block) || !huge_pool_test(huge_pool_free_bits_, block));
        huge_pool_set(huge_pool_free_bits_, block, false);
        huge_pool_free_count_--;
        huge_pool_hint_ = block / HUGE_POOL_WORD_BITS;
    }

    /* Off the lock, a page at a time: the pages not known zero zeroed, and
       under zerofree=verify the others read */
    const ulong phys = block * HUGE_FRAME_SIZE;
    for (ulong i = zero_check_ ? 0 : done; i < HUGE_FRAME_PAGES; i++)
    {
        if (i < done)
            check_huge_page_zeroed(phys + i * consts::PAGE_SIZE);
        else
            zero_huge_page(phys + i * consts::PAGE_SIZE);
    }
    return phys;
}

void PageTable::free_huge_frame(ulong phy_addr)
{
    ulong block = 0;
    if (phy_addr % HUGE_FRAME_SIZE != 0 || !huge_pool_block_of(phy_addr, block))
        PANIC("free_huge_frame: 0x%lX is no block of the huge pool", phy_addr);

    stdlib::AutoLock lock(huge_pool_lock_);
    if (huge_pool_test(huge_pool_free_bits_, block))
        PANIC("free_huge_frame: block 0x%lX freed twice", phy_addr);
    BUG_ON(huge_pool_free_count_ >= huge_pool_count_);
    /* Only a free block is known zero */
    BUG_ON(huge_pool_test(huge_pool_zeroed_bits_, block));
    huge_pool_set(huge_pool_free_bits_, block, true);
    huge_pool_free_count_++;
}

bool PageTable::zero_free_huge_page()
{
    stdlib::AutoLock lock(huge_pool_lock_);
    if (huge_zero_block_ == NO_HUGE_BLOCK)
    {
        ulong block = 0;
        /* Every free block known zero, or none free: nothing to do */
        if (huge_pool_zeroed_count_ == huge_pool_free_count_)
            return false;
        if (!huge_pool_find(huge_pool_free_bits_, huge_pool_zeroed_bits_, block))
            PANIC("zero_free_huge_page: %lu blocks free and %lu known zero by the counts, none other by the bitmaps",
                huge_pool_free_count_, huge_pool_zeroed_count_);
        huge_zero_block_ = block;
        huge_zero_next_ = 0;
    }

    /* Free, and not known zero, for as long as it is the zeroing's: an
       allocation of it ends that (alloc_huge_frame) */
    BUG_ON(!huge_pool_test(huge_pool_free_bits_, huge_zero_block_) ||
        huge_pool_test(huge_pool_zeroed_bits_, huge_zero_block_));
    zero_huge_page(huge_zero_block_ * HUGE_FRAME_SIZE + huge_zero_next_ * consts::PAGE_SIZE);
    huge_zero_next_++;
    if (huge_zero_next_ == HUGE_FRAME_PAGES)
    {
        huge_pool_set(huge_pool_zeroed_bits_, huge_zero_block_, true);
        huge_pool_zeroed_count_++;
        huge_zero_block_ = NO_HUGE_BLOCK;
    }
    return true;
}

ulong PageTable::get_huge_pool_blocks()
{
    /* Set once, by reserve_huge_pool, before there is another CPU. */
    return huge_pool_count_;
}

ulong PageTable::get_huge_pool_free()
{
    stdlib::AutoLock lock(huge_pool_lock_);
    return huge_pool_free_count_;
}

ulong PageTable::get_huge_pool_zeroed()
{
    stdlib::AutoLock lock(huge_pool_lock_);
    return huge_pool_zeroed_count_;
}

PtePage* PageTable::walk_to_l2_locked(ulong virt_addr)
{
    if (root_ == 0)
        return nullptr;

    PtePage* table = (PtePage*)tmp_map_page(root_);
    if (table == nullptr)
        return nullptr;

    for (ulong level = 4; level > 2; level--)
    {
        ulong index = (level == 4) ? Pte::l4_index(virt_addr) : Pte::l3_index(virt_addr);
        Pte* entry = &table->entry[index];
        if (entry->present() && entry->huge())
        {
            /* A 1GiB block where a table belongs: its GiB is somebody's */
            tmp_unmap_page((ulong)table);
            return nullptr;
        }
        if (!entry->present())
        {
            Page* page = alloc_page_no_lock();
            if (page == nullptr)
            {
                tmp_unmap_page((ulong)table);
                return nullptr;
            }
            entry->set_address(page->get_phy_address());
            entry->set_writable();
            /* No NX on the way down, as walk_to_l1_locked has it: execute
               permission is a leaf's to take away. */
            entry->set_present();
        }

        PtePage* next = (PtePage*)tmp_map_page(entry->address());
        tmp_unmap_page((ulong)table);
        if (next == nullptr)
            return nullptr;
        table = next;
    }
    return table;
}

/* The window's top-level entry was empty before map_huge_pool_window, so
   every table under it is the window's own: the entry cleared first, so
   that the walker reaches none of them, this CPU's TLB -- the only one
   there is yet -- flushed of whatever of them it cached, and only then
   each L2 table and the L3 table back on the free list. A tmp_map slot at a
   time: a walk cut short for want of a second slot is one way here. */
void PageTable::unmap_huge_pool_window_locked()
{
    PtePage* top = (PtePage*)tmp_map_page(root_);
    if (top == nullptr)
        PANIC("mm: no tmp_map slot to undo the huge pool's window");
    Pte* l4_entry = &top->entry[Pte::l4_index(HUGE_POOL_WINDOW)];
    const bool present = l4_entry->present();
    const ulong l3_phys = l4_entry->address();
    l4_entry->clear();
    tmp_unmap_page((ulong)top);
    hal::tlb_flush_all();
    if (!present)
        return;

    PtePage* l3 = (PtePage*)tmp_map_page(l3_phys);
    if (l3 == nullptr)
        PANIC("mm: no tmp_map slot to undo the huge pool's window");
    for (size_t i = 0; i < stdlib::array_size(l3->entry); i++)
    {
        if (!l3->entry[i].present())
            continue;
        /* Each a table of the window's, made by walk_to_l2_locked */
        BUG_ON(l3->entry[i].huge());
        free_page_no_lock(&page_array_[l3->entry[i].address() / consts::PAGE_SIZE]);
    }
    tmp_unmap_page((ulong)l3);
    free_page_no_lock(&page_array_[l3_phys / consts::PAGE_SIZE]);
}

bool PageTable::map_huge_pool_window()
{
    static_assert(HUGE_POOL_WINDOW == MemoryMap::KERNEL_SPACE_BASE + 64 * 1024 * consts::GB,
        "the window 64 TiB into the kernel's half, as page_table.h says");

    stdlib::AutoLock lock(lock_);
    BUG_ON(huge_pool_window_on_);
    if (huge_pool_count_ == 0)
        return false;

    /* Its top-level entry its own: nothing else is mapped under it, and the
       undoing of a window cut short frees every table there. */
    PtePage* top = (PtePage*)tmp_map_page(root_);
    if (top == nullptr)
    {
        klog(0, "mm: no tmp_map slot for the huge pool's window: guests' memory is copied through frame slots");
        return false;
    }
    const bool taken = top->entry[Pte::l4_index(HUGE_POOL_WINDOW)].present();
    tmp_unmap_page((ulong)top);
    if (taken)
    {
        klog(0, "mm: the huge pool's window's top-level entry is taken: guests' memory is copied through frame slots");
        return false;
    }

    const ulong blocks = stdlib::min(page_array_count_ / HUGE_FRAME_PAGES, HUGE_POOL_MAX_BLOCKS);
    ulong mapped = 0;
    for (ulong block = 0; block < blocks; block++)
    {
        if (!huge_pool_test(huge_pool_in_, block))
            continue;
        const ulong phys = block * HUGE_FRAME_SIZE;
        const ulong va = HUGE_POOL_WINDOW + phys;
        PtePage* l2 = walk_to_l2_locked(va);
        if (l2 == nullptr)
        {
            unmap_huge_pool_window_locked();
            klog(0, "mm: the huge pool's window cut short at block %lu of %lu: guests' memory is copied through "
                "frame slots", mapped, huge_pool_count_);
            return false;
        }
        Pte* entry = &l2->entry[Pte::l2_index(va)];
        /* A table of the window's own, made empty: nothing there yet */
        BUG_ON(entry->present());
        /* Built aside and stored whole, so the walker never sees half of it */
        Pte e;
        e.set_address(phys);
        e.set_writable();
        e.set_no_execute();
        e.set_huge();
        e.set_present();
        entry->value = e.value;
        tmp_unmap_page((ulong)l2);
        mapped++;
    }
    /* Not present before, so no TLB holds anything of them: only the
       stores to make the walker's. */
    hal::pte_made_valid();
    huge_pool_window_on_ = true;
    klog(0, "mm: the huge pool's %lu blocks mapped at 0x%lX on: guests' memory is copied through them", mapped,
        HUGE_POOL_WINDOW);
    return true;
}

ulong PageTable::huge_pool_window(ulong phy_addr)
{
    ulong block = 0;
    if (!huge_pool_window_on_ || !huge_pool_block_of(phy_addr, block))
        return 0;
    return HUGE_POOL_WINDOW + phy_addr;
}

/* Sentinel for tmp_map_page_array_ when page was mapped without a Page struct (e.g. reserved ACPI region). */
static Page* const TMP_MAP_DIRECT_SENTINEL = (Page*)1;

/* A slot is flushed of what it held on the CPU that maps it and on the CPU
   that unmaps it, and on no other: the caller is held to one CPU from the
   map to the unmap (preempt_disable_task, a spinlock, interrupts off), or a
   move half way leaves the translation cached where the next user of the
   slot reads and writes through it. What maps once for good -- the VGA
   buffer, the local APIC, the IO-APIC -- does it before the scheduler
   preempts, which is before any other CPU runs. Either is checked at both
   ends: a caller that could be moved is a panic at its first call. */
ulong PageTable::tmp_map_page(ulong phy_addr)
{
    BUG_ON(preempt_can_move());

    stdlib::AutoLock lock(tmp_map_lock_);

    BUG_ON(phy_addr & (consts::PAGE_SIZE - 1));

    bool have_page = (phy_addr / consts::PAGE_SIZE) < page_array_count_;

    for (size_t i = 0; i < stdlib::array_size(tmp_map_page_array_); i++)
    {
        if (!tmp_map_page_array_[i]) {
            ulong virt_addr = tmp_map_start_ + i * consts::PAGE_SIZE;
            ulong l1_index = Pte::l1_index(virt_addr);
            Pte *l1_entry = &tmp_map_l1_page_->entry[l1_index];
            if (l1_entry->present())
                return 0;

            l1_entry->set_address(phy_addr);
            /* tmp_map serves both RAM (free pages, ACPI tables) and MMIO
               (LAPIC, IOAPIC): only non-RAM gets mapped uncached. */
            if (!MemoryMap::get_instance().is_usable_ram(phy_addr))
                l1_entry->set_cache_disabled();
            l1_entry->set_writable();
            /* The window exists to read and write page tables, page contents
               and device registers; nothing is ever fetched from it. */
            l1_entry->set_no_execute();
            l1_entry->set_present();
            hal::tlb_flush_page(virt_addr);

            if (have_page) {
                Page* page = get_page(phy_addr);
                BUG_ON(!page);
                tmp_map_page_array_[i] = page;
            } else {
                /* Map reserved physical range (e.g. ACPI tables) without a Page struct. */
                tmp_map_page_array_[i] = TMP_MAP_DIRECT_SENTINEL;
            }
            return virt_addr;
        }
    }

    return 0;
}

ulong PageTable::frame_slot_address(ulong cpu)
{
    /* Every CPU has a slot of its own: a CPU's index is the hardware ID the
       CPU table is indexed by, and below MAX_CPUS. */
    static_assert(FRAME_SLOT_COUNT >= MAX_CPUS, "a frame slot for every CPU");
    static_assert(TMP_MAP_SHARED_COUNT != 0, "shared slots for tmp_map_page");
    return tmp_map_start_ + (TMP_MAP_SHARED_COUNT + cpu) * consts::PAGE_SIZE;
}

ulong PageTable::map_frame_slot(ulong phy_addr)
{
    /* The slot is this CPU's alone only while nothing can interrupt the copy
       on it, nor move the copy to another CPU. */
    BUG_ON(hal::is_interrupt_enabled());
    BUG_ON(!is_frame_address(phy_addr));

    ulong cpu = hal::get_current_cpu_hw_id();
    BUG_ON(cpu >= FRAME_SLOT_COUNT);
    ulong virt_addr = frame_slot_address(cpu);
    Pte* l1_entry = &tmp_map_l1_page_->entry[Pte::l1_index(virt_addr)];

    /* Cleared and flushed by the last copy on this CPU: present, it is a copy
       that never unmapped, or two copies at once. */
    BUG_ON(l1_entry->present());

    /* Built aside and stored whole, so the walker never sees half of it. */
    Pte entry;
    entry.set_address(phy_addr);
    entry.set_writable();
    entry.set_no_execute();
    entry.set_present();
    l1_entry->value = entry.value;
    /* It was not present, so no TLB holds anything of it: nothing to
       invalidate, only the store to make the walker's. */
    hal::pte_made_valid();
    return virt_addr;
}

void PageTable::unmap_frame_slot(ulong virt_addr)
{
    BUG_ON(hal::is_interrupt_enabled());
    ulong cpu = hal::get_current_cpu_hw_id();
    BUG_ON(cpu >= FRAME_SLOT_COUNT || virt_addr != frame_slot_address(cpu));

    Pte* l1_entry = &tmp_map_l1_page_->entry[Pte::l1_index(virt_addr)];
    BUG_ON(!l1_entry->present());
    l1_entry->value = 0;
    /* The frame is mapped nowhere again once this returns, and the next copy
       on this CPU finds the slot with nothing of it cached. */
    hal::tlb_flush_page(virt_addr);
}

ulong PageTable::tmp_unmap_page(ulong virt_addr)
{
    BUG_ON(preempt_can_move()); /* as tmp_map_page */

    stdlib::AutoLock lock(tmp_map_lock_);

    BUG_ON(virt_addr & (consts::PAGE_SIZE - 1));
    BUG_ON(!virt_addr);
    BUG_ON(virt_addr < tmp_map_start_);
    BUG_ON(virt_addr >= (tmp_map_start_ + stdlib::array_size(tmp_map_page_array_) * consts::PAGE_SIZE));
    size_t i = (virt_addr - tmp_map_start_) / consts::PAGE_SIZE;
    Page* page = tmp_map_page_array_[i];
    BUG_ON(!page);

    ulong l1_index = Pte::l1_index(virt_addr);
    Pte *l1_entry = &tmp_map_l1_page_->entry[l1_index];
    BUG_ON(!l1_entry->present());
    auto phy_addr = l1_entry->address();
    l1_entry->clear();
    hal::tlb_flush_page(virt_addr);
    tmp_map_page_array_[i] = nullptr;
    if (page != TMP_MAP_DIRECT_SENTINEL) {
        BUG_ON(page->get_phy_address() != phy_addr);
        page->put();
    }

    return phy_addr;
}

ulong PageTable::get_root()
{
    /*
     * Root is set once in setup() before any AP starts
     * and never changes — no lock needed.  Taking the
     * SpinLock here is unsafe for APs that call get_root()
     * before set_cr3: SpinLock::lock() invokes get_boot_time()
     * which reads kvmclock pv_clock_ memory through a VA that
     * only resolves correctly under the kernel PageTable,
     * not the boot page tables the AP still uses.
     */
    return root_;
}

void PageTable::invalidate_local_tlb()
{
    hal::tlb_flush_all();
}

void PageTable::invalidate_local_tlb_address(ulong virt_addr)
{
    hal::tlb_flush_page(virt_addr);
}

void PageTable::invalidate_local_tlb_range(ulong virt_addr, ulong count)
{
    for (ulong i = 0; i < count; i++)
        hal::tlb_flush_page(virt_addr + i * consts::PAGE_SIZE);
}

ulong PageTable::tmp_map_address(ulong phy_addr)
{
    ulong phy_page = phy_addr & ~(consts::PAGE_SIZE - 1);
    ulong va_page = tmp_map_page(phy_page);
    if (!va_page)
        return 0;

    return va_page + (phy_addr - phy_page);
}

ulong PageTable::tmp_map_range(ulong phy_addr, size_t len)
{
    ulong phy_base = phy_addr & ~(consts::PAGE_SIZE - 1);
    ulong offset = phy_addr - phy_base;
    size_t page_count = (offset + len + consts::PAGE_SIZE - 1) / consts::PAGE_SIZE;

    if (page_count <= 1)
        return tmp_map_address(phy_addr);

    stdlib::AutoLock lock(tmp_map_lock_);

    for (size_t i = 0; i + page_count <= stdlib::array_size(tmp_map_page_array_); i++)
    {
        bool all_free = true;
        for (size_t j = 0; j < page_count; j++)
        {
            if (tmp_map_page_array_[i + j])
            {
                all_free = false;
                i += j; /* skip to after the occupied slot */
                break;
            }
        }
        if (!all_free)
            continue;

        for (size_t j = 0; j < page_count; j++)
        {
            ulong this_phy_page = phy_base + j * consts::PAGE_SIZE;
            ulong virt_addr = tmp_map_start_ + (i + j) * consts::PAGE_SIZE;
            ulong l1_index = Pte::l1_index(virt_addr);
            Pte *l1_entry = &tmp_map_l1_page_->entry[l1_index];
            if (l1_entry->present())
                return 0;

            l1_entry->set_address(this_phy_page);
            if (!MemoryMap::get_instance().is_usable_ram(this_phy_page))
                l1_entry->set_cache_disabled();
            l1_entry->set_writable();
            l1_entry->set_no_execute(); /* data, as in tmp_map_page */
            l1_entry->set_present();
            hal::tlb_flush_page(virt_addr);

            bool have_page = (this_phy_page / consts::PAGE_SIZE) < page_array_count_;
            if (have_page) {
                Page* page = get_page(this_phy_page);
                BUG_ON(!page);
                tmp_map_page_array_[i + j] = page;
            } else {
                tmp_map_page_array_[i + j] = TMP_MAP_DIRECT_SENTINEL;
            }
        }
        return tmp_map_start_ + i * consts::PAGE_SIZE + offset;
    }

    return 0;
}

Page* PageTable::source_page(const MapSource& src, size_t index)
{
    if (src.ptrs != nullptr)
        return src.ptrs[index];

    if (src.array != nullptr)
        return &src.array[index];

    /* get_page's reference is only for the lookup; map_range_locked takes the
       mapping reference of its own and releases this one. */
    return get_page(src.phy_addrs[index]);
}

/* Walk down to the L1 table that maps virt_addr and return it temp-mapped,
   or nullptr. With create set, a missing L4/L3/L2 entry is filled in with
   a fresh table; without it a missing level means the caller asked about a
   VA that was never mapped, which is a bug in the caller. Lock must be
   held, and the caller tmp_unmap_page()s the result. */
PtePage* PageTable::walk_to_l1_locked(ulong virt_addr, bool create)
{
    /* No BUG_ON: map_page answered a missing Root with a plain failure, and
       a walk can also fail on a legitimately exhausted tmp_map. */
    if (root_ == 0)
        return nullptr;

    PtePage* table = (PtePage*)tmp_map_page(root_);
    if (table == nullptr)
        return nullptr;

    /* L4 -> L3 -> L2; each step consumes that level's slice of the VA and
       leaves the next table temp-mapped in its place. */
    for (ulong level = 4; level > 1; level--)
    {
        ulong index = (level == 4) ? Pte::l4_index(virt_addr)
                    : (level == 3) ? Pte::l3_index(virt_addr)
                                   : Pte::l2_index(virt_addr);

        Pte* entry = &table->entry[index];
        if (!entry->present())
        {
            if (!create)
            {
                BUG_ON(1);
                tmp_unmap_page((ulong)table);
                return nullptr;
            }

            Page* page = alloc_page_no_lock();
            if (page == nullptr)
            {
                tmp_unmap_page((ulong)table);
                return nullptr;
            }

            entry->set_address(page->get_phy_address());
            entry->set_writable();
            /* No NX on the way down: on both arches the bit on an L4/L3/L2
               entry takes away execute permission from the whole subtree,
               which no leaf can give back -- a module's text lives under
               these tables. Execute permission is a leaf decision. */
            entry->set_present();
        }

        PtePage* next = (PtePage*)tmp_map_page(entry->address());
        tmp_unmap_page((ulong)table);
        if (next == nullptr)
            return nullptr;

        table = next;
    }

    return table;
}

/* The one range mapper behind map_page and the three MapXxxPages forms.
   The walk is redone only when the run steps into the next L1 table, which
   for a naturally aligned block of at most MAX_CONTIGUOUS_PAGES pages never
   happens. Lock must be held. */
bool PageTable::map_range_locked(ulong virt_addr, size_t count, const MapSource& src)
{
    BUG_ON(virt_addr & (consts::PAGE_SIZE - 1));

    PtePage* l1_page = nullptr;
    size_t mapped = 0;

    for (size_t i = 0; i < count; i++)
    {
        ulong va = virt_addr + i * consts::PAGE_SIZE;
        ulong l1_index = Pte::l1_index(va);

        BUG_ON(va >= tmp_map_start_ && va < (tmp_map_start_ + TMP_MAP_PAGE_COUNT * consts::PAGE_SIZE));

        if (l1_page == nullptr)
        {
            l1_page = walk_to_l1_locked(va, true);
            if (l1_page == nullptr)
                break;
        }

        Pte* l1_entry = &l1_page->entry[l1_index];
        if (l1_entry->present())
            break;

        Page* page = source_page(src, i);
        if (page != nullptr)
        {
            page->get();
            l1_entry->set_address(page->get_phy_address());
            l1_entry->set_writable();
            /* RW+NX is what every runtime mapping gets: the heap, stacks,
               DMA buffers and a module image before it is laid out. The one
               thing that runs from such a page is a module's text, and the
               loader asks for it by name (set_range_protection) once the
               segment is written -- so nothing is ever writable and
               executable at the same time. */
            l1_entry->set_no_execute();
            l1_entry->set_present();
            if (src.phy_addrs != nullptr)
                page->put(); /* balance source_page's get_page */
        }
        else
        {
            l1_entry->clear();
        }

        hal::tlb_flush_page(va);
        mapped++;

        /* Last entry of this table: the next VA starts a new walk. */
        if (l1_index == (stdlib::array_size(l1_page->entry) - 1))
        {
            tmp_unmap_page((ulong)l1_page);
            l1_page = nullptr;
        }
    }

    if (l1_page != nullptr)
        tmp_unmap_page((ulong)l1_page);

    if (mapped == count)
        return true;

    /* All-or-nothing: undo the prefix that did get mapped, leaving the
       pages themselves to the caller. */
    if (mapped != 0)
        unmap_range_locked(virt_addr, mapped, nullptr);

    return false;
}

/* Range twin of map_range_locked. A missing entry is a caller bug (BUG_ON),
   but the walk carries on so the rest of the range is still torn down.
   Lock must be held. */
void PageTable::unmap_range_locked(ulong virt_addr, size_t count, Page** frames)
{
    BUG_ON(virt_addr & (consts::PAGE_SIZE - 1));

    PtePage* l1_page = nullptr;

    for (size_t i = 0; i < count; i++)
    {
        ulong va = virt_addr + i * consts::PAGE_SIZE;
        ulong l1_index = Pte::l1_index(va);

        BUG_ON(va >= tmp_map_start_ && va < (tmp_map_start_ + TMP_MAP_PAGE_COUNT * consts::PAGE_SIZE));

        if (l1_page == nullptr)
        {
            /* A missing level is a BUG_ON in the walk, so this is a window
               with no slot for it: the pages from va on would stay mapped
               with their references while the caller -- map_range_locked's
               undo among them -- goes on as if they were not */
            l1_page = walk_to_l1_locked(va, false);
            if (l1_page == nullptr)
                PANIC("unmap_range_locked: no tmp_map slot to walk to 0x%lX, %lu pages left mapped",
                    va, (ulong)(count - i));
        }

        Pte* l1_entry = &l1_page->entry[l1_index];
        if (!l1_entry->present())
        {
            BUG_ON(1);
            /* No page of this address's for the caller to free */
            if (frames != nullptr)
                frames[i] = nullptr;
        }
        else
        {
            ulong phy_addr = l1_entry->address();
            l1_entry->clear();
            BUG_ON(!phy_addr);
            hal::tlb_flush_page(va);

            Page* page = get_page(phy_addr);
            page->put(); /* balance get_page's lookup reference */
            page->put(); /* the mapping reference map_range_locked took */
            /* The caller's to free, after the shootdown (unmap_pages) */
            if (frames != nullptr)
                frames[i] = page;
        }

        if (l1_index == (stdlib::array_size(l1_page->entry) - 1))
        {
            tmp_unmap_page((ulong)l1_page);
            l1_page = nullptr;
        }
    }

    if (l1_page != nullptr)
        tmp_unmap_page((ulong)l1_page);
}

bool PageTable::map_page(ulong virt_addr, Page* page)
{
    stdlib::AutoLock lock(lock_);

    MapSource src = { &page, nullptr, nullptr };

    return map_range_locked(virt_addr, 1, src);
}

bool PageTable::map_pages(ulong virt_addr, Page* const* pages, size_t count)
{
    stdlib::AutoLock lock(lock_);

    MapSource src = { pages, nullptr, nullptr };

    return map_range_locked(virt_addr, count, src);
}

bool PageTable::map_contiguous_pages(ulong virt_addr, Page* pages, size_t count)
{
    stdlib::AutoLock lock(lock_);

    MapSource src = { nullptr, pages, nullptr };

    return map_range_locked(virt_addr, count, src);
}

bool PageTable::map_phys_pages(ulong virt_addr, const ulong* phy_addrs, size_t count)
{
    stdlib::AutoLock lock(lock_);

    MapSource src = { nullptr, nullptr, phy_addrs };

    return map_range_locked(virt_addr, count, src);
}

void PageTable::unmap_pages(ulong virt_addr, size_t count, Page** frames)
{
    stdlib::AutoLock lock(lock_);

    unmap_range_locked(virt_addr, count, frames);
}

void PageTable::free_pages(Page* const* pages, size_t count)
{
    stdlib::AutoLock lock(lock_);

    for (size_t i = 0; i < count; i++)
    {
        /* None where unmap_pages found nothing mapped */
        if (pages[i] != nullptr)
            free_page_no_lock(pages[i]);
    }
}

ulong PageTable::map_mmio_region(ulong phys_addr, ulong size_bytes, MmioCachePolicy policy)
{
    static_assert(MMIO_WINDOW == MemoryMap::KERNEL_SPACE_BASE + 48 * 1024 * consts::GB,
        "the window 48 TiB into the kernel's half, as page_table.h says");
    static_assert(IMAGE_ENTRY_END == MemoryMap::KERNEL_SPACE_BASE + TOP_ENTRY_SPAN,
        "the image's top-level entry is the kernel half's first");

    const bool write_combining =
        (policy == MmioWriteCombining) && hal::is_write_combining_available();

    /* The arch's premapped window is device-typed, so it can only serve an
       uncached request; a write-combining one builds its own leaf PTEs. */
    if (policy == MmioUncached)
    {
        ulong premapped = hal::mmio_premapped_va(phys_addr, size_bytes);
        if (premapped != 0)
            return premapped;
    }

    if (phys_addr & (consts::PAGE_SIZE - 1))
        return 0;

    /* A BAR's or the firmware's word for where the device is: a range that
       wraps, or reaches past what a table entry can name, is none */
    if (size_bytes > MemoryMap::MAX_PHYS_ADDR || phys_addr > MemoryMap::MAX_PHYS_ADDR - size_bytes)
        return 0;

    ulong pages = (size_bytes + consts::PAGE_SIZE - 1) / consts::PAGE_SIZE;
    if (pages == 0)
        pages = 1;

    ulong va = 0;
    {
        stdlib::AutoLock lock(lock_);

        if (root_ == 0)
            return 0;

        /* Mapped already, all of it and as asked: that mapping */
        for (ulong i = 0; i < mmio_mapping_count_; i++)
        {
            const MmioMapping& m = mmio_mappings_[i];
            if (m.write_combining != write_combining || phys_addr < m.phys || pages > m.pages)
                continue;
            const ulong skipped = (phys_addr - m.phys) / consts::PAGE_SIZE;
            if (skipped <= m.pages - pages)
                return m.va + (phys_addr - m.phys);
        }

        /* Its pages and one more, left unmapped. The addresses are taken
           whether or not all of it is then mapped: nobody is handed them
           again, so no later mapping finds a leaf of this one in its way. */
        const ulong left = MMIO_WINDOW + TOP_ENTRY_SPAN - mmio_next_;
        if (pages >= left / consts::PAGE_SIZE)
            return 0;
        va = mmio_next_;
        mmio_next_ += (pages + 1) * consts::PAGE_SIZE;
    }

    /* An L1 table's worth under the lock at a time: a framebuffer is tens of
       thousands of pages, and the lock keeps interrupts off */
    for (ulong done = 0; done < pages;)
    {
        stdlib::AutoLock lock(lock_);

        const ulong page_va = va + done * consts::PAGE_SIZE;
        PtePage* l1_page = walk_to_l1_locked(page_va, true);
        if (l1_page == nullptr)
            return 0;

        for (ulong l1_index = Pte::l1_index(page_va);
            done < pages && l1_index < stdlib::array_size(l1_page->entry); done++, l1_index++)
        {
            Pte* l1_entry = &l1_page->entry[l1_index];
            /* The window's addresses are handed out once each */
            BUG_ON(l1_entry->present());

            /* Built aside and stored whole, so the walker never sees half
               of it */
            Pte e;
            e.set_address(phys_addr + done * consts::PAGE_SIZE);
            if (write_combining)
                e.set_write_combining();
            else
                e.set_cache_disabled();   /* device memory: uncached */
            /* Nothing is ever executed out of MMIO */
            e.set_no_execute();
            e.set_writable();
            e.set_present();
            l1_entry->value = e.value;
        }
        tmp_unmap_page((ulong)l1_page);
    }

    /* Not present before, so no TLB holds anything of them: only the stores
       to make the walker's */
    hal::pte_made_valid();

    {
        stdlib::AutoLock lock(lock_);

        if (mmio_mapping_count_ < MMIO_MAPPINGS_MAX)
        {
            MmioMapping& m = mmio_mappings_[mmio_mapping_count_++];
            m.phys = phys_addr;
            m.pages = pages;
            m.va = va;
            m.write_combining = write_combining;
        }
    }

    return va;
}

/* Legacy single-page form: returns the page so the caller can pair its own
   free_page/Put with it, the way it did before unmap_pages existed. */
Page* PageTable::unmap_page(ulong virt_addr)
{
    stdlib::AutoLock lock(lock_);

    PtePage* l1_page = walk_to_l1_locked(virt_addr, false);
    if (l1_page == nullptr)
        return nullptr;

    BUG_ON(virt_addr & (consts::PAGE_SIZE - 1));
    BUG_ON(virt_addr >= tmp_map_start_ && virt_addr < (tmp_map_start_ + TMP_MAP_PAGE_COUNT * consts::PAGE_SIZE));

    Pte* l1_entry = &l1_page->entry[Pte::l1_index(virt_addr)];
    if (!l1_entry->present())
    {
        BUG_ON(1);
        tmp_unmap_page((ulong)l1_page);
        return nullptr;
    }

    ulong phy_addr = l1_entry->address();
    l1_entry->clear();
    tmp_unmap_page((ulong)l1_page);
    BUG_ON(!phy_addr);
    hal::tlb_flush_page(virt_addr);

    Page* page = get_page(phy_addr);
    page->put();
    return page;
}

ulong PageTable::get_free_pages_count()
{
    stdlib::AutoLock lock(lock_);
    return free_pages_count_;
}

ulong PageTable::get_free_pages_lowest()
{
    stdlib::AutoLock lock(lock_);
    return free_pages_lowest_;
}

ulong PageTable::get_total_pages_count()
{
    stdlib::AutoLock lock(lock_);
    return total_pages_count_;
}

ulong PageTable::get_va_end()
{
    /* Rounded to the huge page, not to the 4KiB one: page_array_ is mapped in
       2MiB blocks, and the VA allocator starts here. Handing it the tail of
       a block that is already mapped would have it build 4KiB mappings under
       an L2 entry that is a leaf. */
    return stdlib::round_up((ulong)&page_array_[page_array_count_], HUGE_PAGE_SIZE);
}

}
}