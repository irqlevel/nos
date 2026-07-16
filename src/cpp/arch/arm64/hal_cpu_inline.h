#pragma once

#include <include/types.h>

// arm64 bodies for the Hal:: CPU wrappers (see hal/cpu.h).
// Included only via hal/cpu.h.

namespace Hal
{

static inline __attribute__((always_inline)) bool IsInterruptEnabled()
{
    ulong daif;
    asm volatile("mrs %0, daif" : "=r"(daif));
    return (daif & (1UL << 7)) == 0; /* I bit clear = IRQs enabled */
}

// Returns the pre-disable IRQ state and disables interrupts. The value is
// opaque to callers except that bit 63 is guaranteed unused by the arch
// (DAIF occupies bits 9:6 only), so kernel code may stash one flag there.
static inline __attribute__((always_inline)) ulong IrqSave()
{
    ulong daif;
    asm volatile("mrs %0, daif" : "=r"(daif));
    asm volatile("msr daifset, #2" ::: "memory");
    return daif;
}

static inline __attribute__((always_inline)) void IrqRestore(ulong flags)
{
    asm volatile("msr daif, %0" :: "r"(flags) : "memory");
}

static inline __attribute__((always_inline)) ulong GetSp()
{
    ulong sp;
    asm volatile("mov %0, sp" : "=r"(sp));
    return sp;
}

static inline __attribute__((always_inline)) void SetSp(ulong newValue)
{
    asm volatile("mov sp, %0" :: "r"(newValue));
}

static inline __attribute__((always_inline)) ulong GetFp()
{
    ulong fp;
    asm volatile("mov %0, x29" : "=r"(fp));
    return fp;
}

// Monotonic per-CPU cycle counter for cheap timestamps/IDs/entropy,
// not a calibrated clock (use GetBootTime for time).
static inline __attribute__((always_inline)) u64 ReadCycleCounter()
{
    u64 cnt;
    asm volatile("isb; mrs %0, cntvct_el0" : "=r"(cnt));
    return cnt;
}

}
