#include <hal/console.h>
#include <hal/power.h>

#include "pl011.h"
#include "board.h"

/* arm64 backends for the HAL console and power services (x86 twin:
   arch/x86_64/hal_x86.cpp). PL011 output is polled in both paths for now;
   the interrupt-driven console arrives with the GIC milestone. */

namespace
{

/* PSCI 0.2 function IDs */
const u32 PsciSystemOff = 0x84000008;
const u32 PsciSystemReset = 0x84000009;

void PsciCall(u32 fn)
{
    register ulong x0 asm("x0") = fn;
    if (Kernel::Board::GetInstance().PsciUseHvc)
        asm volatile("hvc #0" : "+r"(x0) :: "x1", "x2", "x3", "memory");
    else
        asm volatile("smc #0" : "+r"(x0) :: "x1", "x2", "x3", "memory");
}

}

namespace Hal
{

void ConsoleWrite(const char *msg)
{
    Kernel::Pl011::PrintString(msg);
}

void ConsolePanicWrite(const char *msg)
{
    Kernel::Pl011::PrintString(msg);
}

void PowerOff()
{
    PsciCall(PsciSystemOff);
    for (;;)
    {
        asm volatile("wfi");
    }
}

void Reset()
{
    PsciCall(PsciSystemReset);
    for (;;)
    {
        asm volatile("wfi");
    }
}

}
