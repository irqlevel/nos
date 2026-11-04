#pragma once

#include <include/types.h>

// x86_64 bodies for the hal:: CPU wrappers (see hal/cpu.h).
// Included only via hal/cpu.h.

#ifdef __cplusplus
extern "C"
{
#endif

ulong get_rflags(void);
void set_rflags(ulong rflags);
ulong get_rsp(void);
void set_rsp(ulong new_value);
ulong get_rbp(void);
u64 read_tsc();

#ifdef __cplusplus
}
#endif

namespace hal
{

/* RFLAGS.IF: whether the CPU takes maskable interrupts. */
constexpr ulong RFLAGS_INTERRUPTS = 1UL << 9;

/* The IRQ wrappers and the cycle counter are instructions in place, where
   asm.asm's functions of the same work were each a -mcmodel=large call --
   a MOVABS and an indirect CALL -- and a RET: on the tracepoints' write
   path, four of them a record (docs/tracing.md, "What a tracepoint costs"). */
static inline __attribute__((always_inline)) bool is_interrupt_enabled()
{
    ulong flags;
    asm volatile("pushfq\n\t"
                 "popq %0"
                 : "=r"(flags)
                 :
                 : "memory");
    return (flags & RFLAGS_INTERRUPTS) ? true : false;
}

// Returns the pre-disable IRQ state and disables interrupts. The value is
// opaque to callers except that bit 63 is guaranteed unused by the arch
// (RFLAGS bit 63 is reserved-zero), so kernel code may stash one flag there.
// "memory": nothing the section guards is moved out of it by the compiler.
static inline __attribute__((always_inline)) ulong irq_save()
{
    ulong flags;
    asm volatile("pushfq\n\t"
                 "popq %0\n\t"
                 "cli"
                 : "=r"(flags)
                 :
                 : "memory");
    return flags;
}

/* Interrupts as `flags` had them, by STI or CLI. Not POPF: a microcoded
   instruction, and IF is the only flag of RFLAGS the pair is for. Nothing
   in the kernel sets another one between the two -- no STD, STAC or TF;
   the string routines clear DF themselves -- and the compiler keeps no
   arithmetic flag across an asm. The STI's one-instruction shadow is
   harmless: an interrupt pending at it is taken after the next instruction,
   and irq_save() begins with a PUSHFQ, so an STI followed at once by a
   CLI still leaves it a window. */
static inline __attribute__((always_inline)) void irq_restore(ulong flags)
{
    if (flags & RFLAGS_INTERRUPTS)
        asm volatile("sti" ::: "memory");
    else
        asm volatile("cli" ::: "memory");
}

static inline __attribute__((always_inline)) ulong get_sp()
{
    return get_rsp();
}

static inline __attribute__((always_inline)) void set_sp(ulong new_value)
{
    set_rsp(new_value);
}

static inline __attribute__((always_inline)) ulong get_fp()
{
    return get_rbp();
}

// Monotonic per-CPU cycle counter for cheap timestamps/IDs/entropy,
// not a calibrated clock (use get_boot_time for time). RDTSC in place, as
// asm.asm's read_tsc is it: not ordered against the instructions around it.
static inline __attribute__((always_inline)) u64 read_cycle_counter()
{
    u32 low, high;
    asm volatile("rdtsc" : "=a"(low), "=d"(high));
    return ((u64)high << 32) | low;
}

// RDTSC is unordered already (see hal/cpu.h): the same read.
static inline __attribute__((always_inline)) u64 read_cycle_counter_unordered()
{
    return read_cycle_counter();
}

// Halts this CPU for good, touching no memory -- no stack, no data: one
// loop of two instructions, so that a wakeup only halts it again. HLT ends,
// even with interrupts off, at an NMI or a machine check, whose handlers
// return here. For a CPU whose stack and everything else may be freed
// under it (Cpu::park).
static inline __attribute__((always_inline, noreturn)) void halt_forever()
{
    asm volatile("1: hlt\n\t"
                 "jmp 1b" ::: "memory");
    __builtin_unreachable();
}

}
