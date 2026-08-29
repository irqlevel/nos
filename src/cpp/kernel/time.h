#pragma once

#include <lib/stdlib.h>

namespace Kernel
{
    void TimeInit();

    Stdlib::Time GetBootTime();

    /* Nanoseconds spanned by a Hal::ReadCycleCounter() delta, or 0 while
       the counter's rate is still unknown (x86 before TSC calibration).
       Lets a hot path timestamp with the raw counter -- one instruction --
       and pay for the conversion only when it has something to report. */
    ulong CycleCounterDeltaToNs(u64 delta);

    void BusyWait(ulong nanoSecs);

    ulong GetWallTimeSecs();
}
