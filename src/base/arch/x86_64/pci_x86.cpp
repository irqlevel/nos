#include <hal/pci.h>

#include <arch/x86_64/asm.h>
#include <drivers/acpi.h>
#include <kernel/klog.h>
#include <mm/memory_map.h>
#include <mm/page_table.h>

/* x86 PCI config backend: the legacy port CAM (0xCF8 address / 0xCFC data)
   for the first 256 bytes of a function's space, which every x86 has, and
   the MCFG's ECAM window for the rest -- the extended space, PCIe's, where
   the Resizable BAR, ACS and AER are -- a bus at a time, once opened
   (pci_extended_open) and found to read what the CAM reads. Firmware
   programs the BARs (SeaBIOS, UEFI), so resource assignment is a no-op. */

namespace
{

/* What the CAM addresses: 256 buses of 32 devices of 8 functions, 256 bytes
   each; and what ECAM does: 4 KiB a function, a MiB a bus */
const u16 BUSES = 256;
const u16 SLOTS = 32;
const u16 FUNCS = 8;
const u16 CAM_BYTES = 256;
const u16 ECAM_BYTES = 4096;
const ulong ECAM_BUS_BYTES = 1UL << 20;
const ulong ECAM_SLOT_SHIFT = 15;
const ulong ECAM_FUNC_SHIFT = 12;
const u32 ALL_ONES = 0xFFFFFFFFU;

/* Each bus's ECAM mapping, 0 for one not opened: written once, at the scan,
   read by every CPU after */
ulong ecam_bus_va[BUSES];

bool addressable(u16 bus, u16 slot, u16 func)
{
    return bus < BUSES && slot < SLOTS && func < FUNCS;
}

u32 config_address(u16 bus, u16 slot, u16 func, u16 offset)
{
    return (u32)(((u32)bus << 16) | ((u32)slot << 11) | ((u32)func << 8) |
                 (offset & 0xfc) | 0x80000000U);
}

/* Where an extended access of the function goes: its dword's address in
   the bus's mapping -- ECAM's memory, where an access is to be aligned to
   its size, so a word or a byte is made at the field itself -- or 0 where
   the bus has none, or the function or offset is none there is */
volatile u8* extended_at(u16 bus, u16 slot, u16 func, u16 offset)
{
    if (!addressable(bus, slot, func) || offset >= ECAM_BYTES)
        return nullptr;
    const ulong va = __atomic_load_n(&ecam_bus_va[bus], __ATOMIC_ACQUIRE);
    if (va == 0)
        return nullptr;
    return reinterpret_cast<volatile u8*>(va + ((ulong)slot << ECAM_SLOT_SHIFT) +
                                          ((ulong)func << ECAM_FUNC_SHIFT) + offset);
}

}

namespace hal
{

u32 pci_config_read32(u16 bus, u16 slot, u16 func, u16 offset)
{
    if (offset >= CAM_BYTES)
    {
        volatile u8* at = extended_at(bus, slot, func, (u16)(offset & 0xFFC));
        return at ? *reinterpret_cast<volatile u32*>(at) : ALL_ONES;
    }
    if (!addressable(bus, slot, func))
        return ALL_ONES;
    outl(0xCF8, config_address(bus, slot, func, offset));
    return inl(0xCFC);
}

void pci_config_write32(u16 bus, u16 slot, u16 func, u16 offset, u32 value)
{
    if (offset >= CAM_BYTES)
    {
        volatile u8* at = extended_at(bus, slot, func, (u16)(offset & 0xFFC));
        if (at)
            *reinterpret_cast<volatile u32*>(at) = value;
        return;
    }
    if (!addressable(bus, slot, func))
        return;
    outl(0xCF8, config_address(bus, slot, func, offset));
    outl(0xCFC, value);
}

/* The data port is four bytes wide, each its own: a word or a byte of it
   written is that part of the register alone */
void pci_config_write16(u16 bus, u16 slot, u16 func, u16 offset, u16 value)
{
    if (offset >= CAM_BYTES)
    {
        volatile u8* at = extended_at(bus, slot, func, (u16)(offset & 0xFFE));
        if (at)
            *reinterpret_cast<volatile u16*>(at) = value;
        return;
    }
    if (!addressable(bus, slot, func))
        return;
    outl(0xCF8, config_address(bus, slot, func, offset));
    outw((u16)(0xCFC + (offset & 2)), value);
}

void pci_config_write8(u16 bus, u16 slot, u16 func, u16 offset, u8 value)
{
    if (offset >= CAM_BYTES)
    {
        volatile u8* at = extended_at(bus, slot, func, offset);
        if (at)
            *at = value;
        return;
    }
    if (!addressable(bus, slot, func))
        return;
    outl(0xCF8, config_address(bus, slot, func, offset));
    outb((u16)(0xCFC + (offset & 3)), value);
}

bool pci_extended_open(u16 bus, u16 slot, u16 func)
{
    if (!addressable(bus, slot, func))
        return false;
    if (__atomic_load_n(&ecam_bus_va[bus], __ATOMIC_ACQUIRE) != 0)
        return true;
    auto& acpi = kernel::Acpi::get_instance();
    const ulong base = acpi.get_mcfg_base();
    if (base == 0 || bus < acpi.get_mcfg_start_bus() || bus > acpi.get_mcfg_end_bus())
        return false;
    /* The MCFG's word for where the window is, held to the memory map: one
       over RAM is no window, as Linux takes none its map does not reserve */
    const ulong phys = base + (ulong)bus * ECAM_BUS_BYTES;
    if (kernel::mm::MemoryMap::get_instance().has_usable_ram_in(phys, phys + ECAM_BUS_BYTES))
    {
        klog(0, "pci: the MCFG puts bus %lu's ECAM at 0x%lX, over RAM: its extended space not used", (ulong)bus,
            phys);
        return false;
    }
    const ulong va = kernel::mm::PageTable::get_instance().map_mmio_region(phys, ECAM_BUS_BYTES);
    if (va == 0)
        return false;
    /* And held to the CAM: the function's ids, read both ways */
    const u32 ecam = *reinterpret_cast<volatile u32*>(va + ((ulong)slot << ECAM_SLOT_SHIFT) +
                                                      ((ulong)func << ECAM_FUNC_SHIFT));
    outl(0xCF8, config_address(bus, slot, func, 0));
    const u32 cam = inl(0xCFC);
    if (ecam != cam)
    {
        klog(0, "pci: bus %lu's ECAM at 0x%lX reads 0x%X for %02lx:%02lx.%lx, the CAM 0x%X: its extended space "
            "not used", (ulong)bus, phys, ecam, (ulong)bus, (ulong)slot, (ulong)func, cam);
        return false;
    }
    __atomic_store_n(&ecam_bus_va[bus], va, __ATOMIC_RELEASE);
    return true;
}

void pci_assign_resources(void* devices, ulong count, ulong stride)
{
    (void)devices;
    (void)count;
    (void)stride;
}

}
