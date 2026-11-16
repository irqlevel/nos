#pragma once

#include <include/types.h>

/* The vector unit -- x86-64's SSE and AVX registers, arm64's FP/SIMD ones --
   for the kernel's SIMD sections (kernel/simd.h), and for nothing else: the
   C++ is built without it (-mno-sse -mno-80387, -mgeneral-regs-only) and the
   Rust is soft-float, so outside a section no instruction of the kernel's
   touches these registers, and no context switch saves them. Backends in
   arch/x86_64/simd_x86.cpp and arch/arm64/simd_arm64.cpp. */

namespace hal
{

/* What a section may use, as bits -- the values kcore::simd has: on x86-64
   SSE2, SSSE3, AVX and AVX2; on arm64 Advanced SIMD (NEON). */
static const u32 SIMD_SSE2 = 1U << 0;
static const u32 SIMD_SSSE3 = 1U << 1;
static const u32 SIMD_AVX = 1U << 2;
static const u32 SIMD_AVX2 = 1U << 3;
static const u32 SIMD_NEON = 1U << 4;

/* Turn the unit on for the CPU this runs on, early in its bring-up and
   before anything can open a section there; what a section may use on it,
   0 for nothing. x86-64: CR0.EM and TS clear and CR0.MP set, CR4.OSFXSR and
   OSXMMEXCPT, and where the CPU has XSAVE and AVX, CR4.OSXSAVE with XCR0 the
   x87, SSE and AVX. arm64: CPACR_EL1.FPEN, the unit's instructions no
   longer trapped at EL1. */
u32 simd_setup_cpu();

/* At a section's start, preemption off: what `features` -- what
   simd_setup_cpu answered every CPU -- needs of the CPU's state put back.
   x86-64: XCR0 has SSE and AVX again where a guest's entry left it
   without -- hv makes XCR0 its guest's for the length of its run, and
   leaves it so. arm64: nothing. */
void simd_enter(u32 features);

/* At a section's end: every vector register zeroed, so that nothing a
   section computed -- a key's words among it -- stays behind in them. */
void simd_leave(u32 features);

}
