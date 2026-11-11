#include "msix.h"

#include "mmio.h"

#include <include/const.h>
#include <arch/x86_64/idt.h>
#include <arch/x86_64/idt_descriptor.h>
#include <kernel/klog.h>

#include <arch/x86_64/lapic.h>
#include <kernel/irq_balance.h>
#include <lib/stdlib.h>
#include <mm/new.h>
#include <mm/page_table.h>

namespace kernel
{

static const u8 PCI_CAP_ID_MSIX = 0x11;

static const u16 PCI_COMMAND_INTX_DISABLE = (u16)(1 << 10);

static const u16 MSIX_CONTROL_ENABLE = (u16)(1 << 15);
static const u16 MSIX_CONTROL_FUNC_MASK = (u16)(1 << 14);

static const u32 MSIX_ADDR_BASE = 0xFEE00000U;

ulong MsixTable::vector_map_[(MsixTable::VECTOR_COUNT + MsixTable::VECTOR_WORD_BITS - 1) /
    MsixTable::VECTOR_WORD_BITS];

MsixTable::MsixTable()
    : table_(nullptr)
    , count_(0)
    , cap_offset_(0)
    , dev_(nullptr)
    , pci_msix_enabled_(false)
    , entry_vector_(nullptr)
{
}

MsixTable::~MsixTable()
{
    IrqBalance::get_instance().remove_msix(this);

    if (dev_ && cap_offset_ != 0 && pci_msix_enabled_)
    {
        auto& pci = Pci::get_instance();
        u16 mc = pci.read_word(dev_->bus, dev_->slot, dev_->func, (u16)(cap_offset_ + 2));
        mc = (u16)(mc & ~MSIX_CONTROL_ENABLE);
        pci.write_word(dev_->bus, dev_->slot, dev_->func, (u16)(cap_offset_ + 2), mc);

        u16 cmd = pci.read_word(dev_->bus, dev_->slot, dev_->func, 0x04);
        cmd = (u16)(cmd & ~PCI_COMMAND_INTX_DISABLE);
        pci.write_word(dev_->bus, dev_->slot, dev_->func, 0x04, cmd);
    }

    if (entry_vector_)
    {
        /* Each entry's vector back: its IDT entry nobody's again, the
           vector free for another table's. MSI-X is off above -- an entry
           with a vector was enabled, and that turned it on -- so the device
           sends nothing more, and the table's own memory is not touched
           here: it may be in a BAR its driver has let go of. An interrupt
           already on its way is named and dismissed by the vector's dummy
           handler, or comes to whoever has the vector next as one more
           than its device raised, which every handler takes. */
        for (u16 i = 0; i < count_; i++)
        {
            const u8 vector = entry_vector_[i];
            if (vector == 0)
                continue;

            Idt::get_instance().reset_descriptor(vector);
            free_vector(vector);
            entry_vector_[i] = 0;
        }

        mm::free(entry_vector_);
        entry_vector_ = nullptr;
    }

    table_ = nullptr;
    count_ = 0;
    dev_ = nullptr;
    cap_offset_ = 0;
    pci_msix_enabled_ = false;
}

bool MsixTable::map_bar_for_table(Pci::DeviceInfo* dev, u8 bar, ulong offset_in_bar,
    ulong table_bytes)
{
    if (bar >= 6)
        return false;

    auto& pci = Pci::get_instance();
    u32 bar_val = pci.get_bar(dev->bus, dev->slot, dev->func, bar);

    if (bar_val & 1)
    {
        klog(0, "MsixTable: BAR%lu is I/O, not MMIO", (ulong)bar);
        return false;
    }

    ulong phys_addr = bar_val & ~0xFUL;

    bool is64 = ((bar_val & 0x6) == 0x4) && (bar + 1 < 6);
    u32 bar_high = 0;
    if (is64)
    {
        bar_high = pci.get_bar(dev->bus, dev->slot, dev->func, (u8)(bar + 1));
        phys_addr |= ((ulong)bar_high << 32);
    }

    if (phys_addr == 0)
        return false;

    /* Size-probe both halves of a 64-bit BAR; probing only the low half
       computes a bogus size for BARs >= 4 GB */
    pci.write_dword(dev->bus, dev->slot, dev->func, (u16)(0x10 + bar * 4), 0xFFFFFFFF);
    u32 size_mask_low = pci.get_bar(dev->bus, dev->slot, dev->func, bar);
    u32 size_mask_high = 0xFFFFFFFF;
    if (is64)
    {
        pci.write_dword(dev->bus, dev->slot, dev->func, (u16)(0x10 + (bar + 1) * 4), 0xFFFFFFFF);
        size_mask_high = pci.get_bar(dev->bus, dev->slot, dev->func, (u8)(bar + 1));
        pci.write_dword(dev->bus, dev->slot, dev->func, (u16)(0x10 + (bar + 1) * 4), bar_high);
    }
    pci.write_dword(dev->bus, dev->slot, dev->func, (u16)(0x10 + bar * 4), bar_val);

    ulong size_mask = ((ulong)size_mask_high << 32) | (ulong)(size_mask_low & ~0xFU);
    ulong bar_size = ~size_mask + 1;
    if (bar_size == 0)
        bar_size = consts::PAGE_SIZE;

    if (offset_in_bar + table_bytes > bar_size)
    {
        klog(0, "MsixTable: table past BAR end");
        return false;
    }

    klog(0, "MsixTable: BAR%lu phys 0x%lX size 0x%lX table off 0x%lX",
        (ulong)bar, phys_addr, bar_size, offset_in_bar);

    auto& pt = mm::PageTable::get_instance();
    ulong va = pt.map_mmio_region(phys_addr, bar_size);
    if (va == 0)
    {
        klog(0, "MsixTable: map_mmio_region failed");
        return false;
    }

    table_ = (volatile u8*)(va + offset_in_bar);
    return true;
}

bool MsixTable::setup(Pci::DeviceInfo* dev, const ulong* mapped_bars)
{
    if (!dev || !dev->valid)
        return false;

    dev_ = dev;

    auto& pci = Pci::get_instance();
    u8 cap = pci.find_capability(dev->bus, dev->slot, dev->func, PCI_CAP_ID_MSIX);
    if (cap == 0)
    {
        klog(0, "MsixTable: no MSI-X capability");
        table_ = nullptr;
        count_ = 0;
        cap_offset_ = 0;
        return false;
    }

    cap_offset_ = cap;

    u32 cap_dw0 = pci.read_dword(dev->bus, dev->slot, dev->func, cap);
    u16 msg_ctrl = (u16)((cap_dw0 >> 16) & 0xFFFF);
    count_ = (u16)((msg_ctrl & 0x7FFu) + 1);

    if (count_ == 0)
        return false;

    u32 tbl_info = pci.read_dword(dev->bus, dev->slot, dev->func, (u16)(cap + 4));
    u8 bir = (u8)(tbl_info & 7);
    ulong off_in_bar = tbl_info & ~7UL;

    ulong table_bytes = (ulong)count_ * 16;

    entry_vector_ = (u8*)mm::alloc((size_t)count_, 0);
    if (!entry_vector_)
        return false;
    stdlib::mem_set(entry_vector_, 0, (size_t)count_);

    bool mapped = false;
    if (mapped_bars && bir < 6 && mapped_bars[bir] != 0)
    {
        /* The driver's mapping of the BAR, where the table is inside what
           it mapped: the offset is the device's word, and added to the
           mapping's address unchecked it put the table wherever the device
           said, past the mapping's end included. */
        const ulong span = mm::PageTable::get_instance().mmio_mapped_span(mapped_bars[bir]);
        if (off_in_bar <= span && table_bytes <= span - off_in_bar)
        {
            table_ = (volatile u8*)(mapped_bars[bir] + off_in_bar);
            mapped = true;
            klog(0, "MsixTable: reusing BAR%lu VA 0x%lX off 0x%lX",
                (ulong)bir, mapped_bars[bir], off_in_bar);
        }
        else if (span == 0)
        {
            /* No mapping the page table noted the length of: taken for
               none that holds the table */
            klog(0, "MsixTable: BAR%lu's mapping at 0x%lX is of no length the page table knows: the table is "
                "mapped by itself", (ulong)bir, mapped_bars[bir]);
        }
        else
        {
            klog(0, "MsixTable: BAR%lu is mapped at 0x%lX for 0x%lX bytes, the table is 0x%lX at 0x%lX of it: "
                "mapped by itself", (ulong)bir, mapped_bars[bir], span, table_bytes, off_in_bar);
        }
    }

    if (!mapped && !map_bar_for_table(dev, bir, off_in_bar, table_bytes))
    {
        mm::free(entry_vector_);
        entry_vector_ = nullptr;
        table_ = nullptr;
        count_ = 0;
        return false;
    }

    for (u16 i = 0; i < count_; i++)
        mask(i);

    klog(0, "MsixTable: %lu entries cap 0x%lX", (ulong)count_, (ulong)cap);
    return true;
}

u8 MsixTable::alloc_vector()
{
    for (ulong i = 0; i < VECTOR_COUNT; i++)
    {
        const ulong bit = 1UL << (i % VECTOR_WORD_BITS);
        if (!(__atomic_fetch_or(&vector_map_[i / VECTOR_WORD_BITS], bit, __ATOMIC_ACQ_REL) & bit))
            return (u8)(MSIX_VECTOR_BASE + i);
    }

    return 0;
}

void MsixTable::free_vector(u8 vector)
{
    if (BUG_ON(vector < MSIX_VECTOR_BASE || vector > MSIX_VECTOR_LIMIT))
        return;

    const ulong i = (ulong)vector - MSIX_VECTOR_BASE;
    __atomic_fetch_and(&vector_map_[i / VECTOR_WORD_BITS], ~(1UL << (i % VECTOR_WORD_BITS)), __ATOMIC_ACQ_REL);
}

void MsixTable::disable_legacy_intx(Pci::DeviceInfo* dev)
{
    auto& pci = Pci::get_instance();
    u16 cmd = pci.read_word(dev->bus, dev->slot, dev->func, 0x04);
    cmd = (u16)(cmd | PCI_COMMAND_INTX_DISABLE);
    pci.write_word(dev->bus, dev->slot, dev->func, 0x04, cmd);
}

u8 MsixTable::enable_vector(u16 index, InterruptHandler& handler)
{
    if (!table_ || !dev_ || index >= count_ || cap_offset_ == 0)
        return 0;

    if (entry_vector_[index] != 0)
    {
        /* Enabled already: the vector it has, and this handler at it. The
           one it had was left there -- the stub of a slot that may have been
           given back since, and be nobody's or somebody else's. */
        const u8 had = entry_vector_[index];
        Idt::get_instance().set_descriptor(had, IdtDescriptor::encode(handler.get_handler_fn()));
        handler.on_interrupt_register(0, had);
        return had;
    }

    u8 vector = alloc_vector();
    if (vector == 0)
    {
        klog(0, "MsixTable: out of vectors");
        return 0;
    }

    /* assign_msix takes the IrqBalance lock; it must complete before
       entry_lock_ is taken because balance() nests them the other way
       around (IrqBalance lock -> Retarget -> entry_lock_). */
    u32 apic_id = (u32)IrqBalance::get_instance().assign_msix(this, index);
    u32 addr_low = MSIX_ADDR_BASE | (apic_id << 12);

    volatile u8* entry = table_ + (ulong)index * 16;

    {
        stdlib::AutoLock lock(entry_lock_);

        mmio_write32(entry + 0, addr_low);
        mmio_write32(entry + 4, 0);
        mmio_write32(entry + 8, (u32)vector);
        mmio_write32(entry + 12, 1);
    }

    Idt::get_instance().set_descriptor(vector, IdtDescriptor::encode(handler.get_handler_fn()));
    handler.on_interrupt_register(0, vector);

    {
        stdlib::AutoLock lock(entry_lock_);
        mmio_write32(entry + 12, 0);
    }

    if (!pci_msix_enabled_)
    {
        disable_legacy_intx(dev_);
        auto& pci = Pci::get_instance();
        u16 mc = pci.read_word(dev_->bus, dev_->slot, dev_->func, (u16)(cap_offset_ + 2));
        /* Clear Function Mask too: firmware may have left it set, which
           would keep every vector masked after enable */
        mc = (u16)((mc | MSIX_CONTROL_ENABLE) & ~MSIX_CONTROL_FUNC_MASK);
        pci.write_word(dev_->bus, dev_->slot, dev_->func, (u16)(cap_offset_ + 2), mc);
        pci_msix_enabled_ = true;
    }

    entry_vector_[index] = vector;
    return vector;
}

void MsixTable::mask(u16 index)
{
    if (!table_ || index >= count_)
        return;

    stdlib::AutoLock lock(entry_lock_);

    volatile u8* entry = table_ + (ulong)index * 16;
    u32 ctrl = mmio_read32(entry + 12);
    mmio_write32(entry + 12, ctrl | 1);
}

void MsixTable::unmask(u16 index)
{
    if (!table_ || index >= count_)
        return;

    stdlib::AutoLock lock(entry_lock_);

    volatile u8* entry = table_ + (ulong)index * 16;
    u32 ctrl = mmio_read32(entry + 12);
    mmio_write32(entry + 12, ctrl & ~1u);
}

void MsixTable::retarget(u16 index, u32 apic_id)
{
    if (!table_ || index >= count_ || entry_vector_[index] == 0)
        return;

    stdlib::AutoLock lock(entry_lock_);

    volatile u8* entry = table_ + (ulong)index * 16;

    /* The address must not change while the entry is unmasked */
    u32 ctrl = mmio_read32(entry + 12);
    mmio_write32(entry + 12, ctrl | 1);

    mmio_write32(entry + 0, MSIX_ADDR_BASE | (apic_id << 12));

    mmio_write32(entry + 12, ctrl);
}

}
