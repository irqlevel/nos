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

void cpu_relax(void);
void cpu_halt(void);

void interrupt_enable(void);
void interrupt_disable(void);

void spin_lock_lock(ulong *lock);
void spin_lock_unlock(ulong *lock);

void atomic_inc(volatile long *pvalue);
void atomic_dec(volatile long *pvalue);
void atomic_add(volatile long *pvalue, long delta);
long atomic_read(volatile long *pvalue);
void atomic_write(volatile long *pvalue, long new_value);
long atomic_read_and_dec(volatile long *pvalue);
long atomic_read_and_inc(volatile long *pvalue);

long atomic_cmpxchg(volatile long *pvalue, long exchange, long comparand);

long atomic_test_and_set_bit(volatile long *pvalue, ulong bit);
long atomic_test_and_clear_bit(volatile long *pvalue, ulong bit);
long atomic_test_bit(volatile long *pvalue, ulong bit);

void switch_context(ulong next_rsp, ulong* curr_rsp, void (*callback)(void* ctx), void* ctx);

/* returns_twice: the optimiser must not keep anything in a register across
   a SetJmp that a LongJmp back into it would find changed */
long set_jmp(void *ctx) __attribute__((returns_twice));
__attribute__((noreturn)) void long_jmp(void *ctx, long result);

#ifdef __cplusplus
}
#endif

static inline void cpu_relax(ulong count)
{
    for (ulong i = 0; i < count; i++)
        cpu_relax();
}

namespace hal
{
/* Fabricate the initial stack frame that SwitchContext pops for a
   brand-new task; returns the initial stack pointer. Defined per arch
   (x86: arch/x86_64/hal_x86.cpp, arm64: arch/arm64/cpu_arm64.cpp). */
ulong build_task_frame(ulong stack_top, ulong entry, ulong arg);

/* Frame pointer recorded in a suspended task's SwitchContext frame
   (task->Rsp on x86 points at a Context; arm64 at the asm.S frame), or 0
   when that frame is not all inside [stackBase, stackLimit). The saved
   stack pointer is read without anything holding the task still -- by bt,
   by a report on a join that does not end (Task::Locate) -- so it may be
   one the task has since moved on from, and a frame read outside the
   stack would fault where a stale one inside it only names wrong frames. */
ulong task_saved_frame_pointer(ulong saved_sp, ulong stack_base, ulong stack_limit);

/* Switch to a fresh stack and call fn(ctx) there, never returning.
   Switching SP mid-function is not expressible safely in C++ (the
   compiler may address temporaries SP-relative, arm64 does at -O0), so
   the switch+call is one indivisible per-arch primitive. Used for the
   never-returning idle/boot task bodies. */
void __attribute__((noreturn)) run_on_stack(ulong stack_top, void (*fn)(void*), void* ctx);

/* Execute a guaranteed-undefined instruction (crash/panic testing) */
static inline __attribute__((always_inline)) void undef_instr()
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
static inline __attribute__((always_inline)) bool double_fault()
{
#if defined(__x86_64__)
    static const ulong NON_CANONICAL = 0x8000000000000000UL;
    asm volatile("movq %0, %%rsp\n\t"
                 "pushq %%rax" :: "r"(NON_CANONICAL) : "memory");
    __builtin_unreachable();
#elif defined(__aarch64__)
    return false;
#endif
}
}

#ifdef __cplusplus
namespace hal
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
bool setup_serializing_lfence();

/* What SetupCpuPerformance left a CPU asking for: on x86 with HWP, the
   range of performance levels it now picks its own clock from -- lowest to
   highest, turbo included -- and the level it guarantees, all in the CPU's
   own units, which on a hybrid part are not the same for its two kinds of
   core. All zero where nothing was done. */
struct CpuPerformance
{
    u32 lowest;
    u32 guaranteed;
    u32 highest;
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
bool setup_cpu_performance(CpuPerformance* out);

/* How fast ReadCycleCounter() counts, in Hz: the TSC's rate as the kernel
   calibrated it on x86, CNTFRQ_EL0 on arm64. 0 while it is not known yet.
   Defined per arch. */
ulong cycle_counter_hz();

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
