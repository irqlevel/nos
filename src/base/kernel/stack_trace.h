#pragma once

#include <lib/stdlib.h>

namespace kernel
{
    class StackTrace
    {
    public:
        static size_t capture(ulong stack_base, ulong stack_limit, ulong *frames, size_t max_frames);
        static size_t capture(ulong *frames, size_t max_frames);

        static size_t capture_from(ulong rbp, ulong stack_base, ulong stack_limit, ulong *frames, size_t max_frames);
        static size_t capture_from(ulong rbp, ulong *frames, size_t max_frames);

        /* Walk from `rbp`, with the stack identified by `sp` -- the stack
           pointer of the code being described, which for an interrupt or an
           exception is Context::get_orig_rsp(). The bounds must come from a
           stack pointer: see detect_bounds_and_capture. */
        static size_t capture_from_sp(ulong rbp, ulong sp, ulong *frames, size_t max_frames);
    };
}