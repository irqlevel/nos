#include <hal/pci.h>

#include <arch/x86_64/asm.h>

/* x86 PCI config backend: legacy port CAM (0xCF8 address / 0xCFC data).
   BARs are programmed by firmware (SeaBIOS), so resource assignment is a
   no-op. */

namespace
{

u32 ConfigAddress(u16 bus, u16 slot, u16 func, u16 offset)
{
    return (u32)(((u32)bus << 16) | ((u32)slot << 11) | ((u32)func << 8) |
                 (offset & 0xfc) | 0x80000000U);
}

}

namespace Hal
{

u32 PciConfigRead32(u16 bus, u16 slot, u16 func, u16 offset)
{
    Out(0xCF8, ConfigAddress(bus, slot, func, offset));
    return In(0xCFC);
}

void PciConfigWrite32(u16 bus, u16 slot, u16 func, u16 offset, u32 value)
{
    Out(0xCF8, ConfigAddress(bus, slot, func, offset));
    Out(0xCFC, value);
}

void PciAssignResources(void* devices, ulong count, ulong stride)
{
    (void)devices;
    (void)count;
    (void)stride;
}

}
