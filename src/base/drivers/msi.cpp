#include "msix.h"

#include <kernel/klog.h>
#include <lib/stdlib.h>
#include <mm/new.h>

/* MSI, as a table of one entry (MsixTable::setup_msi): the capability's
   fields, which are the same on both arches -- only what goes in the message
   (a LAPIC's address and a vector, or the ITS's doorbell and an event) is
   the arch TU's (drivers/msix.cpp, arch/arm64/msix_its.cpp). */

namespace kernel
{

static const u8 PCI_CAP_ID_MSI = 0x05;

/* The capability (PCI 3.0, 6.8.1): its message control word, the address
   below 4 GiB, and -- where the device addresses 64 bits -- the address
   above, which moves the data and the mask bits a dword on */
static const u16 MSI_CONTROL = 2;
static const u16 MSI_ADDRESS_LOW = 4;
static const u16 MSI_ADDRESS_HIGH = 8;
static const u16 MSI_DATA_32 = 8;
static const u16 MSI_DATA_64 = 12;
static const u16 MSI_MASK_32 = 12;
static const u16 MSI_MASK_64 = 16;

static const u16 MSI_CONTROL_ENABLE = 1 << 0;
/* Multiple Message Enable, bits 4-6: 0 is one message */
static const u16 MSI_CONTROL_MME_MASK = 7 << 4;
static const u16 MSI_CONTROL_64BIT = 1 << 7;
static const u16 MSI_CONTROL_PER_VECTOR_MASK = 1 << 8;
/* The one vector's bit of the mask register */
static const u32 MSI_MASK_VECTOR0 = 1;
/* The capability at its longest -- 64-bit, with mask and pending bits -- and
   the standard config space it has to lie in: its offset is the device's
   word, and a field past 0xFF would be written somewhere else entirely */
static const ulong MSI_CAP_LONGEST = 24;
static const ulong PCI_CONFIG_STANDARD = 256;

bool MsixTable::setup_msi(Pci::DeviceInfo* dev)
{
    if (!dev || !dev->valid)
        return false;

    auto& pci = Pci::get_instance();
    u8 cap = pci.find_capability(dev->bus, dev->slot, dev->func, PCI_CAP_ID_MSI);
    if (cap == 0)
    {
        klog(0, "MsixTable: %02lx:%02lx.%lx has no MSI capability", (ulong)dev->bus, (ulong)dev->slot,
            (ulong)dev->func);
        return false;
    }
    if ((ulong)cap + MSI_CAP_LONGEST > PCI_CONFIG_STANDARD)
    {
        klog(0, "MsixTable: %02lx:%02lx.%lx says its MSI capability is at 0x%lx, too near the end of its config space",
            (ulong)dev->bus, (ulong)dev->slot, (ulong)dev->func, (ulong)cap);
        return false;
    }

    dev_ = dev;
    cap_offset_ = cap;
    msi_ = true;
    count_ = 1;

    u16 control = pci.read_word(dev->bus, dev->slot, dev->func, (u16)(cap + MSI_CONTROL));
    msi_64_ = (control & MSI_CONTROL_64BIT) != 0;
    msi_maskable_ = (control & MSI_CONTROL_PER_VECTOR_MASK) != 0;

    /* Off, and one message, until a vector is enabled: firmware may have
       left it otherwise */
    control = (u16)(control & ~(MSI_CONTROL_ENABLE | MSI_CONTROL_MME_MASK));
    pci.write_word(dev->bus, dev->slot, dev->func, (u16)(cap + MSI_CONTROL), control);

    entry_vector_ = (u8*)mm::alloc((size_t)count_, 0);
    if (!entry_vector_)
    {
        msi_ = false;
        count_ = 0;
        cap_offset_ = 0;
        dev_ = nullptr;
        return false;
    }
    stdlib::mem_set(entry_vector_, 0, (size_t)count_);

    if (msi_maskable_)
        msi_set_masked(true);

    if (!map_msi_device())
    {
        mm::free(entry_vector_);
        entry_vector_ = nullptr;
        msi_ = false;
        count_ = 0;
        cap_offset_ = 0;
        dev_ = nullptr;
        return false;
    }

    klog(0, "MsixTable: MSI at cap 0x%lx of %02lx:%02lx.%lx, %s, %s", (ulong)cap, (ulong)dev->bus,
        (ulong)dev->slot, (ulong)dev->func, msi_64_ ? "64-bit" : "32-bit",
        msi_maskable_ ? "maskable" : "not maskable");
    return true;
}

void MsixTable::msi_write_message(u64 address, u32 data)
{
    auto& pci = Pci::get_instance();
    const u16 cap = cap_offset_;
    pci.write_dword(dev_->bus, dev_->slot, dev_->func, (u16)(cap + MSI_ADDRESS_LOW), (u32)address);
    if (msi_64_)
    {
        pci.write_dword(dev_->bus, dev_->slot, dev_->func, (u16)(cap + MSI_ADDRESS_HIGH), (u32)(address >> 32));
        pci.write_word(dev_->bus, dev_->slot, dev_->func, (u16)(cap + MSI_DATA_64), (u16)data);
    }
    else
    {
        pci.write_word(dev_->bus, dev_->slot, dev_->func, (u16)(cap + MSI_DATA_32), (u16)data);
    }
}

void MsixTable::msi_set_masked(bool masked)
{
    if (!msi_maskable_)
        return;

    auto& pci = Pci::get_instance();
    const u16 at = (u16)(cap_offset_ + (msi_64_ ? MSI_MASK_64 : MSI_MASK_32));
    u32 bits = pci.read_dword(dev_->bus, dev_->slot, dev_->func, at);
    bits = masked ? (bits | MSI_MASK_VECTOR0) : (bits & ~MSI_MASK_VECTOR0);
    pci.write_dword(dev_->bus, dev_->slot, dev_->func, at, bits);
}

void MsixTable::msi_set_enabled(bool enabled)
{
    auto& pci = Pci::get_instance();
    const u16 at = (u16)(cap_offset_ + MSI_CONTROL);
    u16 control = pci.read_word(dev_->bus, dev_->slot, dev_->func, at);
    control = (u16)(control & ~MSI_CONTROL_MME_MASK);
    control = enabled ? (u16)(control | MSI_CONTROL_ENABLE) : (u16)(control & ~MSI_CONTROL_ENABLE);
    pci.write_word(dev_->bus, dev_->slot, dev_->func, at, control);
}

}
