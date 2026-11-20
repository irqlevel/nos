#pragma once

#include <include/types.h>

#ifdef __cplusplus
extern "C"
{
#endif

struct CpuidResult
{
    u32 eax;
    u32 ebx;
    u32 ecx;
    u32 edx;
};

void cpuid_call(u32 leaf, u32 subleaf, CpuidResult* out);

#ifdef __cplusplus
}
#endif

static inline CpuidResult cpuid(u32 leaf, u32 subleaf = 0)
{
    CpuidResult r;
    cpuid_call(leaf, subleaf, &r);
    return r;
}

/* Who made the CPU: leaf 0's EBX, EDX and ECX, its name in twelve bytes */
enum class CpuVendor
{
    Intel,
    Amd,
    Hygon,
    Other,
};

static inline CpuVendor cpu_vendor(const CpuidResult& leaf0)
{
    /* "GenuineIntel", "AuthenticAMD", "HygonGenuine", as EBX, EDX, ECX give them */
    static const u32 INTEL_EBX = 0x756E6547, INTEL_EDX = 0x49656E69, INTEL_ECX = 0x6C65746E;
    static const u32 AMD_EBX = 0x68747541, AMD_EDX = 0x69746E65, AMD_ECX = 0x444D4163;
    static const u32 HYGON_EBX = 0x6F677948, HYGON_EDX = 0x6E65476E, HYGON_ECX = 0x656E6975;

    if (leaf0.ebx == INTEL_EBX && leaf0.edx == INTEL_EDX && leaf0.ecx == INTEL_ECX)
        return CpuVendor::Intel;
    if (leaf0.ebx == AMD_EBX && leaf0.edx == AMD_EDX && leaf0.ecx == AMD_ECX)
        return CpuVendor::Amd;
    if (leaf0.ebx == HYGON_EBX && leaf0.edx == HYGON_EDX && leaf0.ecx == HYGON_ECX)
        return CpuVendor::Hygon;
    return CpuVendor::Other;
}

static inline CpuVendor cpu_vendor()
{
    return cpu_vendor(cpuid(0));
}

/* The family leaf 1's EAX gives, its extension added where the base
   family is 0Fh (SDM vol. 2, CPUID; AMD's APM vol. 3 the same) */
static inline u32 cpu_family(const CpuidResult& leaf1)
{
    static const u32 FAMILY_SHIFT = 8, FAMILY_MASK = 0xF;
    static const u32 EXT_FAMILY_SHIFT = 20, EXT_FAMILY_MASK = 0xFF;
    static const u32 FAMILY_EXTENDED = 0xF;

    u32 family = (leaf1.eax >> FAMILY_SHIFT) & FAMILY_MASK;
    if (family == FAMILY_EXTENDED)
        family += (leaf1.eax >> EXT_FAMILY_SHIFT) & EXT_FAMILY_MASK;
    return family;
}

static inline u32 cpu_family()
{
    return cpu_family(cpuid(1));
}
