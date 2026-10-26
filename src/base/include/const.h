#pragma once

#include "types.h"

namespace consts
{
    const size_t PAGE_SIZE = 4096;
    const size_t PAGE_SHIFT = 12;

    const size_t SECTOR_SIZE = 512;
    const size_t SECTOR_SHIFT = 9;

    const size_t KB = 1024;
    const size_t MB = 1024 * 1024;
    const size_t GB = 1024 * 1024 * 1024;

    const ulong NANO_SECS_IN_SEC = 1000000000;
    const ulong NANO_SECS_IN_MS = 1000000;
    const ulong NANO_SECS_IN_USEC = 1000;
};
