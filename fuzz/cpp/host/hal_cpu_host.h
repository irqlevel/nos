// The Hal:: CPU wrappers of hal/cpu.h, for a process on the host: the
// interrupt flag is a word the stand-ins keep (kernel.cpp), the stack and
// frame pointers are the host's, and the cycle counter counts.
#pragma once

#include <include/types.h>

namespace fuzz
{
/* The interrupt flag, as the kernel under test last left it. */
extern bool interrupts_on;
extern u64 cycles;
[[noreturn]] void host_hal_unreachable(const char* what);
}

namespace hal
{

static inline bool is_interrupt_enabled()
{
    return fuzz::interrupts_on;
}

/* Bit 0 the flag before; bit 63 is the kernel's to stash one of its own
   in, as on both arches. */
static inline ulong irq_save()
{
    ulong flags = fuzz::interrupts_on ? 1 : 0;
    fuzz::interrupts_on = false;
    return flags;
}

static inline void irq_restore(ulong flags)
{
    fuzz::interrupts_on = (flags & 1) != 0;
}

static inline ulong get_sp()
{
    return reinterpret_cast<ulong>(__builtin_frame_address(0));
}

static inline void set_sp(ulong new_value)
{
    (void)new_value;
    fuzz::host_hal_unreachable("Hal::SetSp");
}

static inline ulong get_fp()
{
    return reinterpret_cast<ulong>(__builtin_frame_address(0));
}

static inline u64 read_cycle_counter()
{
    return ++fuzz::cycles;
}

}
