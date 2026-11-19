#pragma once

#include <include/types.h>

// PCI config-space access backend + resource assignment. x86: legacy port
// CAM (0xCF8/0xCFC) for the first 256 bytes, the MCFG's ECAM past them; BARs
// are firmware-programmed so assignment is a no-op.
// arm64: ECAM (pci-host-ecam-generic), and the kernel must assign BARs from
// the host bridge's MMIO window since bare -kernel boot has no firmware.
namespace hal
{

u32 pci_config_read32(u16 bus, u16 slot, u16 func, u16 offset);
void pci_config_write32(u16 bus, u16 slot, u16 func, u16 offset, u32 value);

/* A word, or a byte, of config space written by itself: nothing else of its
   dword is written. A word's and a byte's write used to be the dword read,
   changed and written back -- and every write-1-to-clear bit beside the
   field was written as it read: a write of the command register cleared
   the status register's error bits next to it, one of a PCIe control
   register its status register's. The word is the one offset is in (its
   bit 0 is not looked at). */
void pci_config_write16(u16 bus, u16 slot, u16 func, u16 offset, u16 value);
void pci_config_write8(u16 bus, u16 slot, u16 func, u16 offset, u8 value);

/* Called once after enumeration with the device array (opaque stride to
   avoid a Pci.h dependency here). x86: no-op. arm64: probe + assign BARs. */
void pci_assign_resources(void* devices, ulong count, ulong stride);

/* The extended configuration space -- offsets 256 to 4095, PCIe's -- of the
   functions on `bus` made reachable, checked on the function at slot.func
   there: true where it is. x86: the MCFG's ECAM window for the bus, mapped
   once and held to the port CAM's reading of that function; till then, and
   where there is none, an extended access reads all ones and writes
   nothing. arm64: ECAM is how every access goes, for the buses mapped. Not
   safe against the accessors on another CPU: the scan calls it, before any
   driver reads */
bool pci_extended_open(u16 bus, u16 slot, u16 func);

}
