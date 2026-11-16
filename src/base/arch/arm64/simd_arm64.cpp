#include <hal/simd.h>

/* arm64's half of hal/simd.h. The C++ around it is built with
   -mgeneral-regs-only, so its assembler refuses an Advanced SIMD instruction
   until a `.arch_extension simd` says it may have them -- which it says
   only in the asm statement that zeroes the registers. */

namespace hal
{

namespace
{

/* ID_AA64PFR0_EL1's FP and AdvSIMD fields: 0xF where the CPU has neither. */
const ulong PFR0_FP_SHIFT = 16;
const ulong PFR0_ADVSIMD_SHIFT = 20;
const ulong PFR0_FIELD_MASK = 0xF;
const ulong PFR0_NOT_IMPLEMENTED = 0xF;

/* CPACR_EL1.FPEN: 0b11, FP/SIMD trapped at neither EL0 nor EL1. ZEN, SVE's,
   stays as it is: trapped, nothing here using SVE. */
const ulong CPACR_FPEN_SHIFT = 20;
const ulong CPACR_FPEN_MASK = 3UL << CPACR_FPEN_SHIFT;
const ulong CPACR_FPEN_NONE_TRAPPED = 3UL << CPACR_FPEN_SHIFT;

}

u32 simd_setup_cpu()
{
    ulong pfr0;
    asm volatile("mrs %0, id_aa64pfr0_el1" : "=r"(pfr0));
    if (((pfr0 >> PFR0_FP_SHIFT) & PFR0_FIELD_MASK) == PFR0_NOT_IMPLEMENTED ||
        ((pfr0 >> PFR0_ADVSIMD_SHIFT) & PFR0_FIELD_MASK) == PFR0_NOT_IMPLEMENTED)
        return 0;

    /* At EL2 boot.S cleared CPTR_EL2.TFP before the drop to EL1, so this is
       the one trap left between EL1 and the unit. */
    ulong cpacr;
    asm volatile("mrs %0, cpacr_el1" : "=r"(cpacr));
    cpacr = (cpacr & ~CPACR_FPEN_MASK) | CPACR_FPEN_NONE_TRAPPED;
    asm volatile("msr cpacr_el1, %0\n\tisb" : : "r"(cpacr) : "memory");
    return SIMD_NEON;
}

void simd_enter(u32 features)
{
    /* Nothing of a section's state can have changed since the setup: no
       guest runs on this CPU (the EL2 hypervisor is not written yet). */
    (void)features;
}

void simd_leave(u32 features)
{
    if (!(features & SIMD_NEON))
        return;
    asm volatile(
        ".arch_extension simd\n\t"
        "movi v0.16b, #0\n\t"
        "movi v1.16b, #0\n\t"
        "movi v2.16b, #0\n\t"
        "movi v3.16b, #0\n\t"
        "movi v4.16b, #0\n\t"
        "movi v5.16b, #0\n\t"
        "movi v6.16b, #0\n\t"
        "movi v7.16b, #0\n\t"
        "movi v8.16b, #0\n\t"
        "movi v9.16b, #0\n\t"
        "movi v10.16b, #0\n\t"
        "movi v11.16b, #0\n\t"
        "movi v12.16b, #0\n\t"
        "movi v13.16b, #0\n\t"
        "movi v14.16b, #0\n\t"
        "movi v15.16b, #0\n\t"
        "movi v16.16b, #0\n\t"
        "movi v17.16b, #0\n\t"
        "movi v18.16b, #0\n\t"
        "movi v19.16b, #0\n\t"
        "movi v20.16b, #0\n\t"
        "movi v21.16b, #0\n\t"
        "movi v22.16b, #0\n\t"
        "movi v23.16b, #0\n\t"
        "movi v24.16b, #0\n\t"
        "movi v25.16b, #0\n\t"
        "movi v26.16b, #0\n\t"
        "movi v27.16b, #0\n\t"
        "movi v28.16b, #0\n\t"
        "movi v29.16b, #0\n\t"
        "movi v30.16b, #0\n\t"
        "movi v31.16b, #0"
        : : : "memory");
}

}
