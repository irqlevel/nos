#pragma once

#include <include/types.h>

namespace Kernel
{

/* SetJmp/LongJmp buffer: x19-x28, x29, x30, sp (+pad to 16-byte multiple) */
struct JmpContext final
{
    ulong X19;
    ulong X20;
    ulong X21;
    ulong X22;
    ulong X23;
    ulong X24;
    ulong X25;
    ulong X26;
    ulong X27;
    ulong X28;
    ulong Fp;   /* x29 */
    ulong Lr;   /* x30 */
    ulong Sp;
    ulong Pad;
};

/* Exception/interrupt register frame, built by vectors.S. The accessor
   names (GetRetRip/GetErrorCode/GetOrigRsp/GetFramePointer) are the
   cross-arch contract with common code. */
struct Context final
{
    ulong X[31];  /* x0..x30 */
    ulong Sp;     /* SP at the exception point (SP_EL1, frame excluded) */
    ulong Elr;    /* return address (ELR_EL1) */
    ulong Spsr;   /* SPSR_EL1 */
    ulong Esr;    /* exception syndrome (ESR_EL1) */
    ulong Far;    /* fault address (FAR_EL1) */

    ulong GetRetRip(bool hasErrorCode = false)
    {
        (void)hasErrorCode;
        return Elr;
    }

    ulong GetErrorCode()
    {
        return Esr;
    }

    ulong GetOrigRsp(bool hasErrorCode = false)
    {
        (void)hasErrorCode;
        return Sp;
    }

    ulong GetFramePointer()
    {
        return X[29];
    }

private:
    Context(const Context& other) = delete;
    Context(Context&& other) = delete;
    Context& operator=(const Context& other) = delete;
    Context& operator=(Context&& other) = delete;
};

static_assert(sizeof(Context) == 36 * 8, "Invalid size");

}
