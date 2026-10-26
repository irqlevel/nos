#pragma once

#include <include/types.h>

namespace kernel
{

/* SetJmp/LongJmp buffer: x19-x28, x29, x30, sp (+pad to 16-byte multiple) */
struct JmpContext final
{
    ulong x19;
    ulong x20;
    ulong x21;
    ulong x22;
    ulong x23;
    ulong x24;
    ulong x25;
    ulong x26;
    ulong x27;
    ulong x28;
    ulong fp;   /* x29 */
    ulong lr;   /* x30 */
    ulong sp;
    ulong pad;
};

/* Exception/interrupt register frame, built by vectors.S. The accessor
   names (GetRetRip/GetErrorCode/GetOrigRsp/GetFramePointer) are the
   cross-arch contract with common code. */
struct Context final
{
    ulong x[31];  /* x0..x30 */
    ulong sp;     /* SP at the exception point (SP_EL1, frame excluded) */
    ulong elr;    /* return address (ELR_EL1) */
    ulong spsr;   /* SPSR_EL1 */
    ulong esr;    /* exception syndrome (ESR_EL1) */
    ulong far;    /* fault address (FAR_EL1) */

    ulong get_ret_rip(bool has_error_code = false)
    {
        (void)has_error_code;
        return elr;
    }

    ulong get_error_code()
    {
        return esr;
    }

    ulong get_orig_rsp(bool has_error_code = false)
    {
        (void)has_error_code;
        return sp;
    }

    ulong get_frame_pointer()
    {
        return x[29];
    }

private:
    Context(const Context& other) = delete;
    Context(Context&& other) = delete;
    Context& operator=(const Context& other) = delete;
    Context& operator=(Context&& other) = delete;
};

static_assert(sizeof(Context) == 36 * 8, "Invalid size");

}
