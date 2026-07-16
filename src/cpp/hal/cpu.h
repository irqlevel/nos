#pragma once

#include <include/types.h>

// Portable CPU primitives. The extern "C" symbols below are the link-level
// HAL contract: every arch defines them (x86_64: arch/x86_64/asm.asm).
// The Hal:: inline functions wrap per-arch state whose native name or
// encoding differs between arches (stack pointer, IRQ flags, cycle counter);
// their bodies live in arch/<arch>/hal_cpu_inline.h.

#ifdef __cplusplus
extern "C"
{
#endif

void Pause(void);
void Hlt(void);

void InterruptEnable(void);
void InterruptDisable(void);

void SpinLockLock(ulong *lock);
void SpinLockUnlock(ulong *lock);

void AtomicInc(volatile long *pvalue);
void AtomicDec(volatile long *pvalue);
void AtomicAdd(volatile long *pvalue, long delta);
long AtomicRead(volatile long *pvalue);
void AtomicWrite(volatile long *pvalue, long newValue);
long AtomicReadAndDec(volatile long *pvalue);
long AtomicReadAndInc(volatile long *pvalue);

long AtomicCmpxchg(volatile long *pvalue, long exchange, long comparand);

long AtomicTestAndSetBit(volatile long *pvalue, ulong bit);
long AtomicTestAndClearBit(volatile long *pvalue, ulong bit);
long AtomicTestBit(volatile long *pvalue, ulong bit);

void SwitchContext(ulong nextRsp, ulong* currRsp, void (*callback)(void* ctx), void* ctx);

long SetJmp(void *ctx);
void LongJmp(void *ctx, long result);

#ifdef __cplusplus
}
#endif

static inline void Pause(ulong count)
{
    for (ulong i = 0; i < count; i++)
        Pause();
}

namespace Hal
{
/* Fabricate the initial stack frame that SwitchContext pops for a
   brand-new task; returns the initial stack pointer. Defined per arch
   (x86: arch/x86_64/hal_x86.cpp, arm64: arch/arm64/cpu_arm64.cpp). */
ulong BuildTaskFrame(ulong stackTop, ulong entry, ulong arg);

/* Frame pointer recorded in a suspended task's SwitchContext frame
   (task->Rsp on x86 points at a Context; arm64 at the asm.S frame). */
ulong TaskSavedFramePointer(ulong savedSp);

/* Switch to a fresh stack and call fn(ctx) there, never returning.
   Switching SP mid-function is not expressible safely in C++ (the
   compiler may address temporaries SP-relative, arm64 does at -O0), so
   the switch+call is one indivisible per-arch primitive. Used for the
   never-returning idle/boot task bodies. */
void __attribute__((noreturn)) RunOnStack(ulong stackTop, void (*fn)(void*), void* ctx);

/* Execute a guaranteed-undefined instruction (crash/panic testing) */
static inline __attribute__((always_inline)) void UndefInstr()
{
#if defined(__x86_64__)
    asm volatile("ud2");
#elif defined(__aarch64__)
    asm volatile("udf #0");
#endif
}
}

// Provides namespace Hal { IsInterruptEnabled, IrqSave, IrqRestore,
// GetSp, SetSp, GetFp, ReadCycleCounter }.
#if defined(__x86_64__)
#include <arch/x86_64/hal_cpu_inline.h>
#elif defined(__aarch64__)
#include <arch/arm64/hal_cpu_inline.h>
#else
#error "unsupported architecture"
#endif
