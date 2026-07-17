#pragma once

#include <include/types.h>

// PCI config-space access backend + resource assignment. x86: legacy port
// CAM (0xCF8/0xCFC), BARs are firmware-programmed so assignment is a no-op.
// arm64: ECAM (pci-host-ecam-generic), and the kernel must assign BARs from
// the host bridge's MMIO window since bare -kernel boot has no firmware.
namespace Hal
{

u32 PciConfigRead32(u16 bus, u16 slot, u16 func, u16 offset);
void PciConfigWrite32(u16 bus, u16 slot, u16 func, u16 offset, u32 value);

/* Called once after enumeration with the device array (opaque stride to
   avoid a Pci.h dependency here). x86: no-op. arm64: probe + assign BARs. */
void PciAssignResources(void* devices, ulong count, ulong stride);

}
