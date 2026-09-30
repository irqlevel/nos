// The Hal:: interrupt-controller wrappers of hal/irqchip.h, for a process on
// the host: no controller, and the CPU the fuzzed code runs on is whichever
// the target says (Fuzz::CurrentCpu). An interrupt or an IPI sent is a
// finding: nothing the fuzzed code does may need one.
#pragma once

#include <include/types.h>

namespace Fuzz
{
extern ulong CurrentCpu;
[[noreturn]] void HostHalUnreachable(const char* what);
}

namespace Hal
{

/* IPI vector: IDT slot on x86, SGI INTID on arm64; on the host, the x86's */
constexpr u8 IpiVector = 0xFE;

static inline bool IrqChipReady()
{
    return true;
}

static inline ulong GetCurrentCpuHwId()
{
    return Fuzz::CurrentCpu;
}

static inline void IrqEoi()
{
}

static inline void IrqEoi(u8 vector)
{
    (void)vector;
}

static inline bool IrqIsInService(u8 vector)
{
    (void)vector;
    return false;
}

static inline void SendIpi(ulong hwId, u8 vector)
{
    (void)hwId;
    (void)vector;
    Fuzz::HostHalUnreachable("Hal::SendIpi");
}

static inline bool NmiIpiSupported()
{
    return false;
}

static inline void SendNmiIpi(ulong hwId)
{
    (void)hwId;
    Fuzz::HostHalUnreachable("Hal::SendNmiIpi");
}

}
