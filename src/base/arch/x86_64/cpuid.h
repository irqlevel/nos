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
