#include "interrupt.h"

namespace Kernel
{

Atomic InterruptStats::Counters[IrqMax];

void InterruptStats::Inc(InterruptSource src)
{
    if (src < IrqMax)
        Counters[src].Inc();
}

long InterruptStats::Get(InterruptSource src)
{
    if (src < IrqMax)
        return Counters[src].Get();
    return 0;
}

}
