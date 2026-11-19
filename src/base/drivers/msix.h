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
       If mapped_bars is non-null, reuses already-mapped BAR VAs (0 = unmapped)
       -- where the table lies inside what is mapped there: the device's word
       for the table's offset is held to the mapping's length. */
    bool setup(Pci::DeviceInfo* dev, const ulong* mapped_bars = nullptr);

    /* The device's MSI capability (cap ID 0x05) taken as a table of one
       entry, for a device that has MSI and no MSI-X -- an Intel AHCI HBA's
       is one. Its entry is the capability's message, written in config
       space rather than in a BAR; everything after -- enable_vector(0),
       disable_vector, mask, unmask, retarget, the destructor -- is the
       table's as for MSI-X. One message (MME 0) whatever the device could
       take. Returns false if the device has no MSI. Does not enable MSI in
       hardware. Never taken in place of MSI-X behind a driver's back: a
       driver asks for it by name (kernel_msi_create). */
    bool setup_msi(Pci::DeviceInfo* dev);

    /* Program table entry `index`, install IDT handler, return CPU vector 0 on failure.
       An entry enabled already keeps its vector and takes the new handler,
       unmasked again: the one it had may be gone (a slot unregistered and
       taken again). */
    u8 enable_vector(u16 index, InterruptHandler& handler);

    /* An enabled entry turned away from its handler, whose slot is being
       given back: masked, and what it sends taken by nobody's handler --
       x86's vector back at the IDT's default, arm64's LPI parked -- the
       vector or the LPI kept for the entry's next enable_vector. Nothing
       for an entry never enabled. */
    void disable_vector(u16 index);

    void mask(u16 index);
    void unmask(u16 index);

    /* Redirect an enabled entry to another CPU (used by IrqBalance) */
    void retarget(u16 index, u32 apic_id);

    u16 get_table_size() const { return count_; }
    bool is_ready() const { return (table_ != nullptr || msi_) && count_ > 0; }

    static const u8 MSIX_VECTOR_BASE = 0x40;
    static const u8 MSIX_VECTOR_LIMIT = 0xEF;

private:
    MsixTable(const MsixTable& other) = delete;
    MsixTable(MsixTable&& other) = delete;
    MsixTable& operator=(const MsixTable& other) = delete;
    MsixTable& operator=(MsixTable&& other) = delete;

    bool map_bar_for_table(Pci::DeviceInfo* dev, u8 bar, ulong offset_in_bar, ulong table_bytes);
    u8 alloc_vector();
    static void free_vector(u8 vector);
    static void disable_legacy_intx(Pci::DeviceInfo* dev);

    /* MSI (setup_msi): the arch's part of taking the device -- arm64 maps it
       with the ITS for one event, x86 has nothing to do -- and the
       capability's own fields, written in config space (drivers/msi.cpp,
       both arches). The message is written with MSI off or the entry
       masked; enabling turns INTx off, as MSI-X's does. */
    bool map_msi_device();
    void msi_write_message(u64 address, u32 data);
    void msi_set_masked(bool masked);
    void msi_set_enabled(bool enabled);

    volatile u8* table_;
    u16 count_;
    u8 cap_offset_;
    Pci::DeviceInfo* dev_;
    bool pci_msix_enabled_;

    /* MSI rather than MSI-X: the capability is its one entry, 64-bit
       addressed or not, maskable per vector or not */
    bool msi_;
    bool msi_64_;
    bool msi_maskable_;

    /* Per-table-entry assigned CPU vector; 0 = none. */
    u8* entry_vector_;

    /* Where interrupts are remapped (x86's VT-d, drivers/iommu.h): each
       entry's handle in its unit's interrupt table, NO_HANDLE for none --
       allocated with the first, and its entries given back with the
       vectors */
    u16* entry_handle_;
    static const u16 NO_HANDLE = 0xFFFF;

    /* The device's requester id: what its remapped entries verify */
    u16 requester_id() const;

    /* Serializes table entry updates (enable_vector/Mask/Unmask/Retarget)
       so a concurrent Mask cannot be clobbered by Retarget restoring a
       stale control value. Not taken from IRQ context. */
    SpinLock entry_lock_;

    /* The vectors handed out, a bit each from MSIX_VECTOR_BASE on: taken
       and given back atomically, so that concurrent device init cannot take
       one twice. A table gives its vectors back as it goes -- they used to
       be counted out and never returned, and a machine was out of them for
       good after as many enables as there are vectors. */
    static const ulong VECTOR_COUNT = MSIX_VECTOR_LIMIT - MSIX_VECTOR_BASE + 1;
    static const ulong VECTOR_WORD_BITS = 8 * sizeof(ulong);
    static ulong vector_map_[(VECTOR_COUNT + VECTOR_WORD_BITS - 1) / VECTOR_WORD_BITS];
};

}
