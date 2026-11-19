#pragma once

#include <include/types.h>

/* The IOMMU as the rest of the kernel reaches it -- the Rust bridge
   (rust_ffi.cpp's kernel_iommu_*), and through it kcore::iommu and hv --
   whichever the architecture has: VT-d on x86-64 (arch/x86_64/vtd.cpp,
   Vtd's domains), none on arm64 yet, where every call says there is none
   (arch/arm64/x86_driver_stubs.cpp). A domain is a handle, 0 for none; a
   requester id is bus << 8 | device << 3 | function. What each does is
   Vtd's to say (vtd.h). */
namespace kernel
{
namespace iommu
{

bool on();
ulong domain_create(u16 sid);
bool domain_map(ulong domain, u64 iova, u64 phys, ulong pages);
bool domain_attach(ulong domain, u16 sid);
bool domain_detach(ulong domain, u16 sid);
void domain_destroy(ulong domain);
bool release(u16 sid);

/* Interrupt remapping (Vtd's is_remapping, remap_*): whether interrupts are
   remapped; an entry for a source, by its requester id -- its handle, or -1
   for none -- moved to another CPU, and given back; the IO-APIC's
   requester id. The kernel's own MSIs, MSI-Xs and IO-APIC pins are
   programmed with a handle while remapping() says so (drivers/msix.cpp,
   arch/x86_64/ioapic.cpp), in the formats below. */
bool remapping();
long remap_alloc(u16 sid, u8 vector, u32 apic_id, bool level);
bool remap_retarget(u16 sid, u16 handle, u32 apic_id);
void remap_free(u16 sid, u16 handle);
u16 io_apic_sid();

/* A remapped interrupt's message (VT-d 5.1.2.2): the local APICs' window,
   the handle's bits 14:0 in the address's 19:5 and its bit 15 in bit 2,
   bit 4 set -- the remappable format -- and no subhandle; the data 0 */
static const u32 MSI_REMAPPED_BASE = 0xFEE00000 | (1u << 4);
inline u32 msi_address(u16 handle)
{
    return MSI_REMAPPED_BASE | (static_cast<u32>(handle & 0x7FFF) << 5) | (static_cast<u32>(handle >> 15) << 2);
}

/* And an IO-APIC entry's (5.1.5.1): bit 48 the remappable format, the
   handle's 14:0 in 63:49 and its bit 15 in bit 11 -- the vector, trigger,
   polarity and mask fields as ever, the vector the entry's own for the
   EOI of a level interrupt to find it */
inline u64 io_apic_entry_bits(u16 handle)
{
    return (1ULL << 48) | (static_cast<u64>(handle & 0x7FFF) << 49) | (static_cast<u64>(handle >> 15) << 11);
}

}
}
