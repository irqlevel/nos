#pragma once

#include <include/types.h>
#include <drivers/pci.h>
#include <kernel/atomic.h>
#include <kernel/interrupt.h>
#include <kernel/spin_lock.h>

namespace kernel
{

class MsixTable
{
public:
    MsixTable();
    ~MsixTable();

    /* Probe PCI config for MSI-X capability (cap ID 0x11). Map MSI-X table.
       Returns false if the device has no MSI-X. Does not enable MSI-X in HW.
       If mapped_bars is non-null, reuses already-mapped BAR VAs (0 = unmapped). */
    bool setup(Pci::DeviceInfo* dev, const ulong* mapped_bars = nullptr);

    /* Program table entry `index`, install IDT handler, return CPU vector 0 on failure. */
    u8 enable_vector(u16 index, InterruptHandler& handler);

    void mask(u16 index);
    void unmask(u16 index);

    /* Redirect an enabled entry to another CPU (used by IrqBalance) */
    void retarget(u16 index, u32 apic_id);

    u16 get_table_size() const { return count_; }
    bool is_ready() const { return table_ != nullptr && count_ > 0; }

    static const u8 MSIX_VECTOR_BASE = 0x40;
    static const u8 MSIX_VECTOR_LIMIT = 0xEF;

private:
    MsixTable(const MsixTable& other) = delete;
    MsixTable(MsixTable&& other) = delete;
    MsixTable& operator=(const MsixTable& other) = delete;
    MsixTable& operator=(MsixTable&& other) = delete;

    bool map_bar_for_table(Pci::DeviceInfo* dev, u8 bar, ulong offset_in_bar, ulong table_bytes);
    u8 alloc_vector();
    static void disable_legacy_intx(Pci::DeviceInfo* dev);

    volatile u8* table_;
    u16 count_;
    u8 cap_offset_;
    Pci::DeviceInfo* dev_;
    bool pci_msix_enabled_;

    /* Per-table-entry assigned CPU vector; 0 = none. */
    u8* entry_vector_;

    /* Serializes table entry updates (enable_vector/Mask/Unmask/Retarget)
       so a concurrent Mask cannot be clobbered by Retarget restoring a
       stale control value. Not taken from IRQ context. */
    SpinLock entry_lock_;

    /* Count of vectors handed out (vector = MSIX_VECTOR_BASE + offset);
       atomic so concurrent device init can't double-allocate */
    static Atomic next_vector_offset_;
};

}
