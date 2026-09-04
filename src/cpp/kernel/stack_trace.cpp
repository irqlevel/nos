#include "stack_trace.h"
#include <hal/cpu.h>
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

    static size_t DetectBoundsAndCapture(ulong rbp, ulong sp, ulong *frames, size_t maxFrames)
    {
        /* The window comes from the stack pointer, never from the frame
           pointer.

           This walk runs on the very stack it is describing -- neither arch
           switches stacks for a same-privilege interrupt, and NMI has no IST
           here -- so the stack pointer is always a real address. The frame
           pointer is not: it is whatever the interrupted code had in RBP, and
           hand-written assembly uses that register for its own purposes
           (asm.asm's RunOnStack loads it from an argument, SetJmp and LongJmp
           save and restore it). A profiler sampling on a performance counter
           lands in exactly that code, because the leaves of a hot path are
           AtomicRead, SetRflags, ReadTsc and Pause.

           Deriving the window from a garbage RBP and then reading the magic
           word out of it dereferences an address that need not be mapped, or
           even canonical, inside an NMI handler -- a fault with nothing left
           to report it. Bounded by the stack pointer instead, a garbage frame
           pointer simply fails CaptureFrom's range check and the walk returns
           no frames, which is the honest answer.

           `sp` is the stack pointer of the code being described, which is not
           always this function's own: #DF is delivered on its own IST stack,
           and its report is about the stack it came from. Callers holding a
           Context pass GetOrigRsp(); the rest pass their own. */
        ulong base = sp & (~(Task::StackSize - 1));
        Task::Stack* stackPtr = reinterpret_cast<Task::Stack*>(base);
        if (stackPtr->Magic1 == Task::StackMagic1 &&
            stackPtr->Magic2 == Task::StackMagic2)
        {
            /* Up to StackTop, not to the end of the struct: the last word is
               Magic2, and a walk allowed to read it reports the guard value
               as a return address -- 0xCBDECBDE appearing as this task's
               outermost caller, which is not a caller at all. */
            return StackTrace::CaptureFrom(rbp, base,
                (ulong)&stackPtr->StackTop[0], frames, maxFrames);
        }

        /* Not a task stack -- a per-CPU static stack, or the boot stack.
           Still bounded by the stack pointer, so still a mapped page. */
        base = sp & (~(Const::PageSize - 1));
        return StackTrace::CaptureFrom(rbp, base, base + Const::PageSize, frames, maxFrames);
    }

    size_t StackTrace::CaptureFrom(ulong rbp, ulong *frames, size_t maxFrames)
    {
        return DetectBoundsAndCapture(rbp, Hal::GetSp(), frames, maxFrames);
    }

    size_t StackTrace::CaptureFromSp(ulong rbp, ulong sp, ulong *frames, size_t maxFrames)
    {
        return DetectBoundsAndCapture(rbp, sp, frames, maxFrames);
    }

    size_t StackTrace::Capture(ulong stackBase, ulong stackLimit, ulong *frames, size_t maxFrames)
    {
        return CaptureFrom(Hal::GetFp(), stackBase, stackLimit, frames, maxFrames);
    }

    size_t StackTrace::Capture(ulong *frames, size_t maxFrames)
    {
        return DetectBoundsAndCapture(Hal::GetFp(), Hal::GetSp(), frames, maxFrames);
    }
}