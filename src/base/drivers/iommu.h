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

}
}
