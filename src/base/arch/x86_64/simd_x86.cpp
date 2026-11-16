#include <hal/simd.h>
#include "asm.h"
#include "cpuid.h"

/* x86-64's half of hal/simd.h. The C++ around it is built without SSE, but
   the assembler takes the instructions in an asm statement all the same --
   the compiler itself just never picks one. */

namespace hal
{

namespace
{

const ulong CR0_MP = 1UL << 1;
const ulong CR0_EM = 1UL << 2;
const ulong CR0_TS = 1UL << 3;
const ulong CR4_OSFXSR = 1UL << 9;
const ulong CR4_OSXMMEXCPT = 1UL << 10;
const ulong CR4_OSXSAVE = 1UL << 18;

/* XCR0's components: the x87's -- always on -- SSE's XMM registers and
   MXCSR, AVX's upper halves of the YMM registers. AVX-512's are left off:
   no section uses them, and no guest is offered them either (hvarch's
   fp.rs), so nothing ever has to save the ZMM registers. */
const u64 XCR0_X87 = 1ULL << 0;
const u64 XCR0_SSE = 1ULL << 1;
const u64 XCR0_AVX = 1ULL << 2;
const u64 XCR0_SECTION = XCR0_X87 | XCR0_SSE | XCR0_AVX;

const u32 LEAF_MAX = 0;
const u32 LEAF_FEATURES = 1;
const u32 LEAF_STRUCTURED = 7;
const u32 LEAF_XSAVE = 0xD;
const u32 FEATURES_EDX_SSE2 = 1U << 26;
const u32 FEATURES_ECX_SSSE3 = 1U << 9;
const u32 FEATURES_ECX_XSAVE = 1U << 26;
const u32 FEATURES_ECX_AVX = 1U << 28;
const u32 STRUCTURED_EBX_AVX2 = 1U << 5;

void write_cr0(ulong value)
{
    asm volatile("mov %0, %%cr0" : : "r"(value) : "memory");
}

void write_cr4(ulong value)
{
    asm volatile("mov %0, %%cr4" : : "r"(value) : "memory");
}

/* XCR0, by XGETBV: CR4.OSXSAVE is set, or it is a #UD. Under a hypervisor
   no exit, where XSETBV always is one. */
u64 read_xcr0()
{
    u32 lo, hi;
    asm volatile("xgetbv" : "=a"(lo), "=d"(hi) : "c"(0));
    return ((u64)hi << 32) | lo;
}

void write_xcr0(u64 value)
{
    asm volatile("xsetbv" : : "a"((u32)value), "d"((u32)(value >> 32)), "c"(0) : "memory");
}

}

u32 simd_setup_cpu()
{
    CpuidResult features = cpuid(LEAF_FEATURES);
    /* Every x86-64 CPU has SSE2; a CPUID that says otherwise is one not to
       trust with the rest either. */
    if (!(features.edx & FEATURES_EDX_SSE2))
        return 0;

    /* SDM 13.1.3: CR0.EM clear, or an SSE instruction is a #UD; CR0.MP set;
       CR0.TS clear, or it is a #NM -- nothing in this kernel sets it. */
    write_cr0((get_cr0() & ~(CR0_EM | CR0_TS)) | CR0_MP);
    write_cr4(get_cr4() | CR4_OSFXSR | CR4_OSXMMEXCPT);
    u32 usable = SIMD_SSE2;
    if (features.ecx & FEATURES_ECX_SSSE3)
        usable |= SIMD_SSSE3;

    /* AVX where the CPU has it and XSAVE's components take it: OSXSAVE, and
       XCR0 the x87, SSE and AVX. AVX2 is an extension of AVX's instructions
       over the same registers, nothing more to turn on. */
    if ((features.ecx & FEATURES_ECX_XSAVE) && (features.ecx & FEATURES_ECX_AVX) &&
        cpuid(LEAF_MAX).eax >= LEAF_XSAVE &&
        (cpuid(LEAF_XSAVE, 0).eax & XCR0_SECTION) == XCR0_SECTION)
    {
        write_cr4(get_cr4() | CR4_OSXSAVE);
        write_xcr0(read_xcr0() | XCR0_SECTION);
        usable |= SIMD_AVX;
        if (cpuid(LEAF_STRUCTURED, 0).ebx & STRUCTURED_EBX_AVX2)
            usable |= SIMD_AVX2;
    }
    return usable;
}

void simd_enter(u32 features)
{
    if (!(features & SIMD_AVX))
        return;
    /* A guest's XCR0 -- what hv left the CPU with after its last exit here
       -- can be without AVX, and then a VEX instruction is a #UD. Its next
       entry makes XCR0 its own again whatever this leaves. */
    u64 xcr0 = read_xcr0();
    if ((xcr0 & XCR0_SECTION) != XCR0_SECTION)
        write_xcr0(xcr0 | XCR0_SECTION);
}

void simd_leave(u32 features)
{
    if (features & SIMD_AVX)
    {
        /* All sixteen YMM registers, upper halves too: and so nothing of
           AVX's state is left dirty for the SSE code of whatever runs on
           this CPU next -- a guest's -- to pay a transition for. */
        asm volatile("vzeroall" : : : "memory");
        return;
    }
    if (!(features & SIMD_SSE2))
        return;
    asm volatile(
        "pxor %%xmm0, %%xmm0\n\t"
        "pxor %%xmm1, %%xmm1\n\t"
        "pxor %%xmm2, %%xmm2\n\t"
        "pxor %%xmm3, %%xmm3\n\t"
        "pxor %%xmm4, %%xmm4\n\t"
        "pxor %%xmm5, %%xmm5\n\t"
        "pxor %%xmm6, %%xmm6\n\t"
        "pxor %%xmm7, %%xmm7\n\t"
        "pxor %%xmm8, %%xmm8\n\t"
        "pxor %%xmm9, %%xmm9\n\t"
        "pxor %%xmm10, %%xmm10\n\t"
        "pxor %%xmm11, %%xmm11\n\t"
        "pxor %%xmm12, %%xmm12\n\t"
        "pxor %%xmm13, %%xmm13\n\t"
        "pxor %%xmm14, %%xmm14\n\t"
        "pxor %%xmm15, %%xmm15"
        : : : "memory");
}

}
