#pragma once

#include <lib/stdlib.h>

namespace Kernel
{
    void TimeInit();

    Stdlib::Time GetBootTime();

    void BusyWait(ulong nanoSecs);

    ulong GetWallTimeSecs();
}
