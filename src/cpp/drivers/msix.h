#pragma once

#include <include/types.h>
#include <drivers/pci.h>
#include <kernel/atomic.h>
#include <kernel/interrupt.h>
#include <kernel/spin_lock.h>

namespace Kernel
{

class MsixTable
{
public:
    MsixTable();
    ~MsixTable();

    /* Probe PCI config for MSI-X capability (cap ID 0x11). Map MSI-X table.
       Returns false if the device has no MSI-X. Does not enable MSI-X in HW.
       If mappedBars is non-null, reuses already-mapped BAR VAs (0 = unmapped). */
    bool Setup(Pci::DeviceInfo* dev, const ulong* mappedBars = nullptr);

    /* Program table entry `index`, install IDT handler, return CPU vector 0 on failure. */
    u8 EnableVector(u16 index, InterruptHandler& handler);

    void Mask(u16 index);
    void Unmask(u16 index);

    /* Redirect an enabled entry to another CPU (used by IrqBalance) */
    void Retarget(u16 index, u32 apicId);

    u16 GetTableSize() const { return Count; }
    bool IsReady() const { return Table != nullptr && Count > 0; }

    static const u8 MsixVectorBase = 0x40;
    static const u8 MsixVectorLimit = 0xEF;

private:
    MsixTable(const MsixTable& other) = delete;
    MsixTable(MsixTable&& other) = delete;
    MsixTable& operator=(const MsixTable& other) = delete;
    MsixTable& operator=(MsixTable&& other) = delete;

    bool MapBarForTable(Pci::DeviceInfo* dev, u8 bar, ulong offsetInBar, ulong tableBytes);
    u8 AllocVector();
    static void DisableLegacyIntx(Pci::DeviceInfo* dev);

    volatile u8* Table;
    u16 Count;
    u8 CapOffset;
    Pci::DeviceInfo* Dev;
    bool PciMsixEnabled;

    /* Per-table-entry assigned CPU vector; 0 = none. */
    u8* EntryVector;

    /* Serializes table entry updates (EnableVector/Mask/Unmask/Retarget)
       so a concurrent Mask cannot be clobbered by Retarget restoring a
       stale control value. Not taken from IRQ context. */
    SpinLock EntryLock;

    /* Count of vectors handed out (vector = MsixVectorBase + offset);
       atomic so concurrent device init can't double-allocate */
    static Atomic NextVectorOffset;
};

}
