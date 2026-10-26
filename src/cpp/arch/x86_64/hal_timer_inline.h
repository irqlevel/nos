#pragma once

#include <include/types.h>
#include <arch/x86_64/lapic.h>

// x86_64 body for hal::local_timer_arm (see hal/timer.h): the local APIC's
// timer, one-shot.

namespace hal
{

static inline __attribute__((always_inline)) void local_timer_arm(ulong delta_ns)
{
    kernel::Lapic::arm_timer(delta_ns);
}

}
