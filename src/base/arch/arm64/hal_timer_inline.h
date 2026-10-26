#pragma once

#include <include/types.h>
#include <arch/arm64/generic_timer.h>

// arm64 body for hal::local_timer_arm (see hal/timer.h): the generic timer's
// CNTV, by its compare value.

namespace hal
{

static inline __attribute__((always_inline)) void local_timer_arm(ulong delta_ns)
{
    kernel::GenericTimer::arm_in(delta_ns);
}

}
