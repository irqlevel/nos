#include "stack_trace.h"
#include "asm.h"
#include "trace.h"
#include "task.h"

namespace Kernel
{
    size_t StackTrace::CaptureFrom(ulong rbp, ulong stackBase, ulong stackLimit, ulong *frames, size_t maxFrames)
    {
        ulong currRbp = rbp;
        size_t i = 0;

        while (i < maxFrames)
        {
            /* RBP must be 8-byte aligned */
            if (currRbp % sizeof(ulong) != 0)
                break;

            /* Both saved-RBP and return-address slots must be in range */
            if (!Stdlib::IsValueInRange(currRbp, stackBase, stackLimit))
                break;
            if (!Stdlib::IsValueInRange(currRbp + sizeof(ulong), stackBase, stackLimit))
                break;

            ulong prevRbp = *(ulong *)(currRbp);
            ulong retAddress = *(ulong *)(currRbp + sizeof(ulong));

            frames[i++] = retAddress;

            /* Stack grows downward: caller's RBP must be strictly higher */
            if (prevRbp <= currRbp)
                break;

            currRbp = prevRbp;
        }

        return i;
    }

    static size_t DetectBoundsAndCapture(ulong rbp, ulong *frames, size_t maxFrames)
    {
        /* Try task stack first (StackSize-aligned, validated by magic) */
        ulong base = rbp & (~(Task::StackSize - 1));
        Task::Stack* stackPtr = reinterpret_cast<Task::Stack*>(base);
        if (stackPtr->Magic1 == Task::StackMagic1 &&
            stackPtr->Magic2 == Task::StackMagic2)
        {
            return StackTrace::CaptureFrom(rbp, base, base + Task::StackSize, frames, maxFrames);
        }

        /* Fallback: conservative page-aligned bounds */
        base = rbp & (~(Const::PageSize - 1));
        return StackTrace::CaptureFrom(rbp, base, base + Const::PageSize, frames, maxFrames);
    }

    size_t StackTrace::CaptureFrom(ulong rbp, ulong *frames, size_t maxFrames)
    {
        return DetectBoundsAndCapture(rbp, frames, maxFrames);
    }

    size_t StackTrace::Capture(ulong stackBase, ulong stackLimit, ulong *frames, size_t maxFrames)
    {
        return CaptureFrom(GetRbp(), stackBase, stackLimit, frames, maxFrames);
    }

    size_t StackTrace::Capture(ulong *frames, size_t maxFrames)
    {
        return DetectBoundsAndCapture(GetRbp(), frames, maxFrames);
    }
}