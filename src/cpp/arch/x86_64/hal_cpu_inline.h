#pragma once

#include <include/types.h>

// x86_64 bodies for the Hal:: CPU wrappers (see hal/cpu.h).
// Included only via hal/cpu.h.

#ifdef __cplusplus
extern "C"
{
#endif

ulong GetRflags(void);
void SetRflags(ulong rflags);
ulong GetRsp(void);
void SetRsp(ulong newValue);
ulong GetRbp(void);
u64 ReadTsc();

#ifdef __cplusplus
}
#endif

namespace Hal
{

static inline __attribute__((always_inline)) bool IsInterruptEnabled()
{
    return (GetRflags() & 0x200) ? true : false;
}

// Returns the pre-disable IRQ state and disables interrupts. The value is
// opaque to callers except that bit 63 is guaranteed unused by the arch
// (RFLAGS bit 63 is reserved-zero), so kernel code may stash one flag there.
static inline __attribute__((always_inline)) ulong IrqSave()
{
    ulong flags = GetRflags();
    InterruptDisable();
    return flags;
}

static inline __attribute__((always_inline)) void IrqRestore(ulong flags)
{
    SetRflags(flags);
}

static inline __attribute__((always_inline)) ulong GetSp()
{
    return GetRsp();
}

static inline __attribute__((always_inline)) void SetSp(ulong newValue)
{
    SetRsp(newValue);
}

static inline __attribute__((always_inline)) ulong GetFp()
{
    return GetRbp();
}

// Monotonic per-CPU cycle counter for cheap timestamps/IDs/entropy,
// not a calibrated clock (use GetBootTime for time).
static inline __attribute__((always_inline)) u64 ReadCycleCounter()
{
    return ReadTsc();
}

}
