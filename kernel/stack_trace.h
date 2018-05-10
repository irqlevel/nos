#pragma once

#include <lib/stdlib.h>

namespace Kernel
{
    class StackTrace
    {
    public:
        static size_t CaptureByRbp(ulong currRbp, ulong stackSize, ulong *frames, size_t maxFrames);
        static size_t Capture(ulong stackSize, ulong *frames, size_t maxFrames);
    };
}
