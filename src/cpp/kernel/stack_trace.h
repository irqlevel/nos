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

        /* Walk from `rbp`, with the stack identified by `sp` -- the stack
           pointer of the code being described, which for an interrupt or an
           exception is Context::GetOrigRsp(). The bounds must come from a
           stack pointer: see DetectBoundsAndCapture. */
        static size_t CaptureFromSp(ulong rbp, ulong sp, ulong *frames, size_t maxFrames);
    };
}