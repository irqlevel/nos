#pragma once

#include <include/types.h>

// x86_64 bodies for the Hal:: CPU wrappers (see hal/cpu.h).
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

static inline __attribute__((always_inline)) bool is_interrupt_enabled()
{
    return (get_rflags() & 0x200) ? true : false;
}

// Returns the pre-disable IRQ state and disables interrupts. The value is
// opaque to callers except that bit 63 is guaranteed unused by the arch
// (RFLAGS bit 63 is reserved-zero), so kernel code may stash one flag there.
static inline __attribute__((always_inline)) ulong irq_save()
{
    ulong flags = get_rflags();
    interrupt_disable();
    return flags;
}

static inline __attribute__((always_inline)) void irq_restore(ulong flags)
{
    set_rflags(flags);
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
// not a calibrated clock (use GetBootTime for time).
static inline __attribute__((always_inline)) u64 read_cycle_counter()
{
    return read_tsc();
}

// Halts this CPU for good, touching no memory -- no stack, no data: one
// loop of two instructions, so that a wakeup only halts it again. HLT ends,
// even with interrupts off, at an NMI or a machine check, whose handlers
// return here. For a CPU whose stack and everything else may be freed
// under it (Cpu::Park).
static inline __attribute__((always_inline, noreturn)) void halt_forever()
{
    asm volatile("1: hlt\n\t"
                 "jmp 1b" ::: "memory");
    __builtin_unreachable();
}

}
