#pragma once

#include <lib/stdlib.h>

namespace kernel
{
    void time_init();

    stdlib::Time get_boot_time();

    /* Nanoseconds spanned by a hal::read_cycle_counter() delta, or 0 while
       the counter's rate is still unknown (x86 before TSC calibration).
       Lets a hot path timestamp with the raw counter -- one instruction --
       and pay for the conversion only when it has something to report. */
    ulong cycle_counter_delta_to_ns(u64 delta);

    void busy_wait(ulong nano_secs);

    ulong get_wall_time_secs();
}
