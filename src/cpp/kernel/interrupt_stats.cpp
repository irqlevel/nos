#include "interrupt.h"

namespace kernel
{

Atomic InterruptStats::counters_[IrqMax];

void InterruptStats::inc(InterruptSource src)
{
    if (src < IrqMax)
        counters_[src].inc();
}

const char* InterruptStats::get_name(InterruptSource src)
{
    switch (src)
    {
    case IrqPit:        return "pit";
    case IrqHpet:       return "hpet";
    case IrqIO8042:     return "8042";
    case IrqSerial:     return "serial";
    case IrqIPI:        return "ipi";
    case IrqLapicTimer: return "lapic-timer";
    case IrqShared:     return "shared";
    case IrqMsix:       return "msix";
    case IrqDummy:      return "dummy";
    case IrqSpurious:   return "spurious";
    case IrqPosted:     return "posted";
    case IrqTimerWake:  return "timer-wake";
    default:            return "unknown";
    }
}

long InterruptStats::get(InterruptSource src)
{
    if (src < IrqMax)
        return counters_[src].get();
    return 0;
}

}
