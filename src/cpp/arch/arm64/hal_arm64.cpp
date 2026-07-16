#include <hal/console.h>
#include <hal/power.h>
#include <hal/cpu.h>
#include <hal/irqchip.h>

#include <lib/printer.h>

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

void PrintCpuState(Stdlib::Printer& con)
{
    ulong v;
    asm volatile("mrs %0, CurrentEL" : "=r"(v));
    con.Printf("el %u sp 0x%p", v >> 2, Hal::GetSp());
    asm volatile("mrs %0, sctlr_el1" : "=r"(v));
    con.Printf(" sctlr 0x%p", v);
    asm volatile("mrs %0, tcr_el1" : "=r"(v));
    con.Printf(" tcr 0x%p\n", v);
    asm volatile("mrs %0, ttbr1_el1" : "=r"(v));
    con.Printf("ttbr1 0x%p", v);
    asm volatile("mrs %0, mair_el1" : "=r"(v));
    con.Printf(" mair 0x%p", v);
    asm volatile("mrs %0, daif" : "=r"(v));
    con.Printf(" daif 0x%p", v);
    asm volatile("mrs %0, vbar_el1" : "=r"(v));
    con.Printf(" vbar 0x%p mpidr %u\n", v, Hal::GetCurrentCpuHwId());
}

void ConsoleOut(const char *s)
{
    Kernel::Pl011::PrintString(s);
}

void ConsoleOutBackspace()
{
    Kernel::Pl011::PrintString("\b \b");
}

void ConsoleOutClear()
{
    /* ANSI escape: clear screen and move cursor home */
    Kernel::Pl011::PrintString("\033[2J\033[H");
}

void ConsoleWrite(const char *msg)
{
    Kernel::Pl011::PrintString(msg);
}

void ConsolePanicWrite(const char *msg)
{
    Kernel::Pl011::PanicPrintString(msg);
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
