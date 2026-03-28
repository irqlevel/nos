#pragma once

#include <lib/stdlib.h>

namespace Kernel
{
    class StackTrace
    {
    public:
        static size_t Capture(ulong stackBase, ulong stackLimit, ulong *frames, size_t maxFrames);
        static size_t Capture(ulong *frames, size_t maxFrames);

        static size_t CaptureFrom(ulong rbp, ulong stackBase, ulong stackLimit, ulong *frames, size_t maxFrames);
        static size_t CaptureFrom(ulong rbp, ulong *frames, size_t maxFrames);
    };
}