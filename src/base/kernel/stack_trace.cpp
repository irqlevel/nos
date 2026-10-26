#include "stack_trace.h"
#include <hal/cpu.h>
#include "trace.h"
#include "task.h"

namespace kernel
{
    size_t StackTrace::capture_from(ulong rbp, ulong stack_base, ulong stack_limit, ulong *frames, size_t max_frames)
    {
        ulong curr_rbp = rbp;
        size_t i = 0;

        while (i < max_frames)
        {
            /* RBP must be 8-byte aligned */
            if (curr_rbp % sizeof(ulong) != 0)
                break;

            /* Both saved-RBP and return-address slots must be in range */
            if (!stdlib::is_value_in_range(curr_rbp, stack_base, stack_limit))
                break;
            if (!stdlib::is_value_in_range(curr_rbp + sizeof(ulong), stack_base, stack_limit))
                break;

            ulong prev_rbp = *(ulong *)(curr_rbp);
            ulong ret_address = *(ulong *)(curr_rbp + sizeof(ulong));

            /* The outermost frame of a task's: nothing called its entry
               function (hal::build_task_frame) */
            if (ret_address == 0)
                break;

            frames[i++] = ret_address;

            /* Stack grows downward: caller's RBP must be strictly higher */
            if (prev_rbp <= curr_rbp)
                break;

            curr_rbp = prev_rbp;
        }

        return i;
    }

    static size_t detect_bounds_and_capture(ulong rbp, ulong sp, ulong *frames, size_t max_frames)
    {
        /* The window comes from the stack pointer, never from the frame
           pointer.

           This walk runs on the very stack it is describing -- neither arch
           switches stacks for a same-privilege interrupt, and NMI has no IST
           here -- so the stack pointer is always a real address. The frame
           pointer is not: it is whatever the interrupted code had in RBP, and
           hand-written assembly uses that register for its own purposes
           (asm.asm's run_on_stack loads it from an argument, set_jmp and long_jmp
           save and restore it). A profiler sampling on a performance counter
           lands in exactly that code, because the leaves of a hot path are
           atomic_read, set_rflags, read_tsc and Pause.

           Deriving the window from a garbage RBP and then reading the magic
           word out of it dereferences an address that need not be mapped, or
           even canonical, inside an NMI handler -- a fault with nothing left
           to report it. Bounded by the stack pointer instead, a garbage frame
           pointer simply fails capture_from's range check and the walk returns
           no frames, which is the honest answer.

           `sp` is the stack pointer of the code being described, which is not
           always this function's own: #DF is delivered on its own IST stack,
           and its report is about the stack it came from. Callers holding a
           Context pass get_orig_rsp(); the rest pass their own. */
        ulong base = sp & (~(Task::STACK_SIZE - 1));
        Task::Stack* stack_ptr = reinterpret_cast<Task::Stack*>(base);
        if (stack_ptr->magic1 == Task::STACK_MAGIC1 &&
            stack_ptr->magic2 == Task::STACK_MAGIC2)
        {
            /* Up to stack_top, not to the end of the struct: the last word is
               Magic2, and a walk allowed to read it reports the guard value
               as a return address -- 0xCBDECBDE appearing as this task's
               outermost caller, which is not a caller at all. */
            return StackTrace::capture_from(rbp, base,
                (ulong)&stack_ptr->stack_top[0], frames, max_frames);
        }

        /* Not a task stack -- a per-CPU static stack, or the boot stack.
           Still bounded by the stack pointer, so still a mapped page. */
        base = sp & (~(consts::PAGE_SIZE - 1));
        return StackTrace::capture_from(rbp, base, base + consts::PAGE_SIZE, frames, max_frames);
    }

    size_t StackTrace::capture_from(ulong rbp, ulong *frames, size_t max_frames)
    {
        return detect_bounds_and_capture(rbp, hal::get_sp(), frames, max_frames);
    }

    size_t StackTrace::capture_from_sp(ulong rbp, ulong sp, ulong *frames, size_t max_frames)
    {
        return detect_bounds_and_capture(rbp, sp, frames, max_frames);
    }

    size_t StackTrace::capture(ulong stack_base, ulong stack_limit, ulong *frames, size_t max_frames)
    {
        return capture_from(hal::get_fp(), stack_base, stack_limit, frames, max_frames);
    }

    size_t StackTrace::capture(ulong *frames, size_t max_frames)
    {
        return detect_bounds_and_capture(hal::get_fp(), hal::get_sp(), frames, max_frames);
    }
}