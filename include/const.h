#pragma once

#include "types.h"

namespace Const
{
    const size_t PageSize = 4096;
    const size_t PageShift = 12;

    const size_t SectorSize = 512;
    const size_t SectorShift = 9;

    const size_t KB = 1024;
    const size_t MB = 1024 * 1024;
    const size_t GB = 1024 * 1024 * 1024;

    const ulong NanoSecsInSec = 1000000000;
    const ulong NanoSecsInMs = 1000000;
    const ulong NanoSecsInUsec = 1000;
};
