#pragma once

#include <include/types.h>

#ifdef __cplusplus
extern "C"
{
#endif

struct CpuidResult
{
    u32 Eax;
    u32 Ebx;
    u32 Ecx;
    u32 Edx;
};

void CpuidCall(u32 leaf, u32 subleaf, CpuidResult* out);

#ifdef __cplusplus
}
#endif

static inline CpuidResult Cpuid(u32 leaf, u32 subleaf = 0)
{
    CpuidResult r;
    CpuidCall(leaf, subleaf, &r);
    return r;
}
