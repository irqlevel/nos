#include "stack_trace.h"
#include "asm.h"
#include "trace.h"

namespace Kernel
{
    size_t StackTrace::Capture(ulong stackSize, ulong *frames, size_t maxFrames)
    {
        ulong currRbp = GetRbp();
        ulong base = currRbp & (~(stackSize - 1));
        ulong limit = base + stackSize;
        size_t i;

        i = 0;
        while(Stdlib::IsValueInRange(currRbp, base, limit)
            && Stdlib::IsValueInRange(currRbp + 1 * sizeof(ulong), base, limit)
            && i < maxFrames)
        {
            ulong *pPrevRbp = (ulong *)(currRbp);
            ulong *pRetAddress = (ulong *)(currRbp + 1 * sizeof(ulong));
            ulong retAddress = *pRetAddress;

            frames[i++] = retAddress;
            currRbp = *pPrevRbp;
        }

        return i;
    }
}