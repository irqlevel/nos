#pragma once

#include <include/types.h>
#include <arch/x86_64/lapic.h>

// x86_64 body for Hal::LocalTimerArm (see hal/timer.h): the local APIC's
// timer, one-shot.

namespace Hal
{

static inline __attribute__((always_inline)) void LocalTimerArm(ulong deltaNs)
{
    Kernel::Lapic::ArmTimer(deltaNs);
}

}
