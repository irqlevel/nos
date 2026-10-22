#pragma once

#include <include/types.h>
#include <arch/arm64/generic_timer.h>

// arm64 body for Hal::LocalTimerArm (see hal/timer.h): the generic timer's
// CNTV, by its compare value.

namespace Hal
{

static inline __attribute__((always_inline)) void LocalTimerArm(ulong deltaNs)
{
    Kernel::GenericTimer::ArmIn(deltaNs);
}

}
