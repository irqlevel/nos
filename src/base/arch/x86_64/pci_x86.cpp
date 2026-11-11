#include <hal/pci.h>

#include <arch/x86_64/asm.h>

/* x86 PCI config backend: legacy port CAM (0xCF8 address / 0xCFC data).
   BARs are programmed by firmware (SeaBIOS), so resource assignment is a
   no-op. */

namespace
{

u32 config_address(u16 bus, u16 slot, u16 func, u16 offset)
{
    return (u32)(((u32)bus << 16) | ((u32)slot << 11) | ((u32)func << 8) |
                 (offset & 0xfc) | 0x80000000U);
}

}

namespace hal
{

u32 pci_config_read32(u16 bus, u16 slot, u16 func, u16 offset)
{
    outl(0xCF8, config_address(bus, slot, func, offset));
    return inl(0xCFC);
}

void pci_config_write32(u16 bus, u16 slot, u16 func, u16 offset, u32 value)
{
    outl(0xCF8, config_address(bus, slot, func, offset));
    outl(0xCFC, value);
}

/* The data port is four bytes wide, each its own: a word or a byte of it
   written is that part of the register alone */
void pci_config_write16(u16 bus, u16 slot, u16 func, u16 offset, u16 value)
{
    outl(0xCF8, config_address(bus, slot, func, offset));
    outw((u16)(0xCFC + (offset & 2)), value);
}

void pci_config_write8(u16 bus, u16 slot, u16 func, u16 offset, u8 value)
{
    outl(0xCF8, config_address(bus, slot, func, offset));
    outb((u16)(0xCFC + (offset & 3)), value);
}

void pci_assign_resources(void* devices, ulong count, ulong stride)
{
    (void)devices;
    (void)count;
    (void)stride;
}

}
