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

/* returns_twice: the optimiser must not keep anything in a register across
   a SetJmp that a LongJmp back into it would find changed */
long SetJmp(void *ctx) __attribute__((returns_twice));
__attribute__((noreturn)) void LongJmp(void *ctx, long result);

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
   (task->Rsp on x86 points at a Context; arm64 at the asm.S frame), or 0
   when that frame is not all inside [stackBase, stackLimit). The saved
   stack pointer is read without anything holding the task still -- by bt,
   by a report on a join that does not end (Task::Locate) -- so it may be
   one the task has since moved on from, and a frame read outside the
   stack would fault where a stale one inside it only names wrong frames. */
ulong TaskSavedFramePointer(ulong savedSp, ulong stackBase, ulong stackLimit);

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

/* A double fault, for real (panic testing): the stack pointer loaded with
   an address no access can use, and a push -- a fault the CPU cannot
   deliver onto that stack either, so it raises #DF, on the stack of its own
   the IST gives it (x86-64; it does not return). arm64 delivers an
   exception onto whatever stack it is on and has no such fault: there it
   returns false, having done nothing. */
static inline __attribute__((always_inline)) bool DoubleFault()
{
#if defined(__x86_64__)
    static const ulong NonCanonical = 0x8000000000000000UL;
    asm volatile("movq %0, %%rsp\n\t"
                 "pushq %%rax" :: "r"(NonCanonical) : "memory");
    __builtin_unreachable();
#elif defined(__aarch64__)
    return false;
#endif
}
}

#ifdef __cplusplus
namespace Hal
{

/* Make LFENCE dispatch-serializing on this CPU where it is not by default,
   so that an `lfence; rdtsc` reads the counter only once every load before
   it has completed. That is the ordering a guest's rdtsc_ordered() counts
   on, and Linux's check that two CPUs' TSCs agree with it: without it an
   RDTSC runs ahead of a load that misses -- a cache line coming from the
   other CCX -- and CPUs whose TSCs agree look hundreds of cycles apart. x86:
   AMD families 10h to 17h and Hygon leave it to DE_CFG (MSR 0xC0011029) bit
   1, which firmware need not set; Linux sets it on every CPU, and so does
   this, on the BSP and on each AP. Nothing to do on Intel, where LFENCE is
   serializing by definition, or under a hypervisor, whose MSR that is.
   arm64: no-op. True when it had to be set: the firmware left it clear.
   Defined per arch. */
bool SetupSerializingLfence();

/* What SetupCpuPerformance left a CPU asking for: on x86 with HWP, the
   range of performance levels it now picks its own clock from -- lowest to
   highest, turbo included -- and the level it guarantees, all in the CPU's
   own units, which on a hybrid part are not the same for its two kinds of
   core. All zero where nothing was done. */
struct CpuPerformance
{
    u32 Lowest;
    u32 Guaranteed;
    u32 Highest;
};

/* Let this CPU run as fast as it can while it is busy, rather than at the
   clock firmware left it at. x86 with hardware P-states (HWP, Skylake and
   later): HWP turned on, and the CPU asked for its whole range, lowest to
   highest, with the clock its own pick at the balanced-performance
   preference -- what Linux's intel_pstate asks by default. Without it an
   Intel CPU stays at the ratio firmware set, its base clock: on the EX44
   (i5-13500) every core ran at 2.5 GHz, its P-cores' turbo being 4.8.
   Nothing under a hypervisor, whose MSRs those are, without HWP, or with
   hwp=off; arm64: nothing. Turning HWP on cannot be undone short of a
   reset. True when HWP was set up, with `out` filled; on the BSP and on
   each AP. Defined per arch. */
bool SetupCpuPerformance(CpuPerformance* out);

/* How fast ReadCycleCounter() counts, in Hz: the TSC's rate as the kernel
   calibrated it on x86, CNTFRQ_EL0 on arm64. 0 while it is not known yet.
   Defined per arch. */
ulong CycleCounterHz();

}
#endif

// Provides namespace Hal { IsInterruptEnabled, IrqSave, IrqRestore,
// GetSp, SetSp, GetFp, ReadCycleCounter }.
#if defined(__x86_64__)
#include <arch/x86_64/hal_cpu_inline.h>
#elif defined(__aarch64__)
#include <arch/arm64/hal_cpu_inline.h>
#else
#error "unsupported architecture"
#endif
