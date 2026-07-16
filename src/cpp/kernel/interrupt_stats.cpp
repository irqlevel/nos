#include "interrupt.h"

namespace Kernel
{

Atomic InterruptStats::Counters[IrqMax];

void InterruptStats::Inc(InterruptSource src)
{
    if (src < IrqMax)
        Counters[src].Inc();
}

const char* InterruptStats::GetName(InterruptSource src)
{
    switch (src)
    {
    case IrqPit:        return "pit";
    case IrqHpet:       return "hpet";
    case IrqIO8042:     return "8042";
    case IrqSerial:     return "serial";
    case IrqVirtioBlk:  return "virtio-blk";
    case IrqVirtioNet:  return "virtio-net";
    case IrqVirtioScsi: return "virtio-scsi";
    case IrqIPI:        return "ipi";
    case IrqShared:     return "shared";
    case IrqMsix:       return "msix";
    case IrqDummy:      return "dummy";
    case IrqSpurious:   return "spurious";
    default:            return "unknown";
    }
}

long InterruptStats::Get(InterruptSource src)
{
    if (src < IrqMax)
        return Counters[src].Get();
    return 0;
}

}
