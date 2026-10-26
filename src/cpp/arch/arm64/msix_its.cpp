#include <drivers/msix.h>

#include "its.h"

#include <drivers/mmio.h>
#include <kernel/trace.h>
#include <lib/stdlib.h>
#include <mm/new.h>
#include <mm/page_table.h>
#include <include/const.h>

/* arm64 MsixTable: same class ABI as drivers/msix.cpp (x86), but MSI is
   routed through the GICv3 ITS instead of a LAPIC-addressed message. The
   MSI-X table setup / BAR mapping is identical; only the message
   composition (address = GITS_TRANSLATER, data = ITS eventId) and the
   interrupt install (an LPI handler, not an IDT vector) differ. This is a
   per-arch TU of the shared class, like CpuTable::StartAll. */

namespace kernel
{

static const u8 PCI_CAP_ID_MSIX = 0x11;
static const u16 PCI_COMMAND_INTX_DISABLE = (u16)(1 << 10);
static const u16 MSIX_CONTROL_ENABLE = (u16)(1 << 15);
static const u16 MSIX_CONTROL_FUNC_MASK = (u16)(1 << 14);

Atomic MsixTable::next_vector_offset_;

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
    if (entry_vector_)
    {
        mm::free(entry_vector_);
        entry_vector_ = nullptr;
    }
    table_ = nullptr;
    count_ = 0;
    dev_ = nullptr;
}

bool MsixTable::map_bar_for_table(Pci::DeviceInfo* dev, u8 bar, ulong offset_in_bar,
    ulong table_bytes)
{
    if (bar >= 6)
        return false;

    auto& pci = Pci::get_instance();
    u32 bar_val = pci.get_bar(dev->bus, dev->slot, dev->func, bar);
    if (bar_val & 1)
        return false;

    ulong phys_addr = bar_val & ~0xFUL;
    bool is64 = ((bar_val & 0x6) == 0x4) && (bar + 1 < 6);
    if (is64)
        phys_addr |= ((ulong)pci.get_bar(dev->bus, dev->slot, dev->func, (u8)(bar + 1)) << 32);
    if (phys_addr == 0)
        return false;

    ulong map_bytes = offset_in_bar + table_bytes;
    map_bytes = (map_bytes + consts::PAGE_SIZE - 1) & ~(consts::PAGE_SIZE - 1);

    ulong va = mm::PageTable::get_instance().map_mmio_region(phys_addr, map_bytes);
    if (va == 0)
        return false;

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
        return false;

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
        table_ = (volatile u8*)(mapped_bars[bir] + off_in_bar);
        mapped = true;
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

    /* Map the device with the ITS up front (RID = deviceId on QEMU virt) */
    u32 device_id = ((u32)dev->bus << 8) | ((u32)dev->slot << 3) | dev->func;
    Its::get_instance().map_device(device_id, count_);

    trace(0, "MsixTable(its): %lu entries cap 0x%lX dev 0x%lX",
        (ulong)count_, (ulong)cap, (ulong)device_id);
    return true;
}

u8 MsixTable::alloc_vector()
{
    /* Unused on arm64 (LPIs are allocated by the ITS); kept for ABI. */
    return 0;
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
        return entry_vector_[index];

    u32 device_id = ((u32)dev_->bus << 8) | ((u32)dev_->slot << 3) | dev_->func;

    u64 msi_addr = 0;
    u32 msi_data = Its::get_instance().map_event(device_id, index, handler, msi_addr);
    if (msi_addr == 0)
    {
        trace(0, "MsixTable(its): MapEvent failed");
        return 0;
    }

    volatile u8* entry = table_ + (ulong)index * 16;
    {
        stdlib::AutoLock lock(entry_lock_);
        mmio_write32(entry + 0, (u32)(msi_addr & 0xFFFFFFFF));
        mmio_write32(entry + 4, (u32)(msi_addr >> 32));
        mmio_write32(entry + 8, msi_data);
        mmio_write32(entry + 12, 0); /* unmasked */
    }

    if (!pci_msix_enabled_)
    {
        disable_legacy_intx(dev_);
        auto& pci = Pci::get_instance();
        u16 mc = pci.read_word(dev_->bus, dev_->slot, dev_->func, (u16)(cap_offset_ + 2));
        mc = (u16)((mc | MSIX_CONTROL_ENABLE) & ~MSIX_CONTROL_FUNC_MASK);
        pci.write_word(dev_->bus, dev_->slot, dev_->func, (u16)(cap_offset_ + 2), mc);
        pci_msix_enabled_ = true;
    }

    /* Non-zero token so re-enable is idempotent; the actual routing token
       is the ITS LPI, not a CPU vector. */
    entry_vector_[index] = (u8)(index + 1);
    return entry_vector_[index];
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
    /* No MSI balancing on arm64 yet (all LPIs on the boot collection) */
    (void)index;
    (void)apic_id;
}

}
