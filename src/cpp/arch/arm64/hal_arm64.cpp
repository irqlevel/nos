#include <hal/console.h>
#include <hal/pmu.h>
#include <hal/power.h>
#include <hal/cpu.h>
#include <hal/irqchip.h>
#include <hal/mmu.h>
#include <hal/module.h>

#include <kernel/elf.h>
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

/* SMCCC: the callee may clobber x1-x17 */
#define SMCCC_CLOBBERS \
    "x1", "x2", "x3", "x4", "x5", "x6", "x7", "x8", "x9", "x10", "x11", \
    "x12", "x13", "x14", "x15", "x16", "x17", "memory"

void PsciCall(u32 fn)
{
    register ulong x0 asm("x0") = fn;
    if (Kernel::Board::GetInstance().PsciUseHvc)
        asm volatile("hvc #0" : "+r"(x0) :: SMCCC_CLOBBERS);
    else
        asm volatile("smc #0" : "+r"(x0) :: SMCCC_CLOBBERS);
}

}

namespace Hal
{

/* PMUv3 exists on the hardware but is not wired up here, and Apple's
   hypervisor does not hand the guest one to test against. */
bool PmuAvailable()
{
    return false;
}

bool PmuStart()
{
    return false;
}

void PmuStop()
{
}

const char* PmuName()
{
    return "none";
}

void PrintCpuInfo(Stdlib::Printer& con)
{
    ulong midr, ctr, mmfr0;
    asm volatile("mrs %0, midr_el1" : "=r"(midr));
    asm volatile("mrs %0, ctr_el0" : "=r"(ctr));
    asm volatile("mrs %0, id_aa64mmfr0_el1" : "=r"(mmfr0));

    ulong implementer = (midr >> 24) & 0xFF;
    const char* name = "unknown";
    switch (implementer)
    {
    case 0x41: name = "ARM"; break;
    case 0x42: name = "Broadcom"; break;
    case 0x43: name = "Cavium"; break;
    case 0x4E: name = "NVIDIA"; break;
    case 0x50: name = "APM"; break;
    case 0x51: name = "Qualcomm"; break;
    case 0x61: name = "Apple"; break;
    case 0x00: name = "reserved/emulated"; break;
    default: break;
    }

    con.Printf("implementer 0x%p (%s)\n", implementer, name);
    con.Printf("part 0x%p variant %u revision %u, midr 0x%p\n",
        (midr >> 4) & 0xFFF, (midr >> 20) & 0xF, midr & 0xF, midr);

    /* Log2 of the words in the smallest cache line, per CTR_EL0. */
    con.Printf("cache line: i %u bytes, d %u bytes\n",
        4UL << (ctr & 0xF), 4UL << ((ctr >> 16) & 0xF));

    /* PARange, the physical address width this core can address. */
    static const u8 ParangeBits[] = { 32, 36, 40, 42, 44, 48, 52, 56 };
    ulong parange = mmfr0 & 0xF;
    con.Printf("physical address bits %u\n",
        (parange < Stdlib::ArraySize(ParangeBits))
            ? (ulong)ParangeBits[parange] : 0UL);
}

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

void EnableWxSupport()
{
    /* PXN/UXN are always active on arm64 — nothing to enable. */
}

void SyncInstructionCache(ulong va, ulong size)
{
    if (size == 0)
        return;

    /* CTR_EL0 DminLine [19:16] and IminLine [3:0]: log2 of the smallest
       data and instruction cache line, in 4-byte words */
    ulong ctr;
    asm volatile("mrs %0, ctr_el0" : "=r"(ctr));
    const ulong dline = 4UL << ((ctr >> 16) & 0xF);
    const ulong iline = 4UL << (ctr & 0xF);
    const ulong end = va + size;

    /* Instruction fetch does not snoop the data cache: push the freshly
       written code out to the point of unification, where fetch sees it... */
    for (ulong p = va & ~(dline - 1); p < end; p += dline)
        asm volatile("dc cvau, %0" :: "r"(p) : "memory");
    asm volatile("dsb ish" ::: "memory");

    /* ...then drop whatever any CPU's instruction cache still holds for these
       lines -- IC IVAU is broadcast to the inner-shareable domain -- and make
       this CPU refetch */
    for (ulong p = va & ~(iline - 1); p < end; p += iline)
        asm volatile("ic ivau, %0" :: "r"(p) : "memory");
    asm volatile("dsb ish" ::: "memory");
    asm volatile("isb" ::: "memory");
}

u16 ModuleElfMachine()
{
    return Kernel::Elf::MachineAarch64;
}

ModuleReloc ClassifyModuleReloc(u32 type)
{
    /* AArch64 ELF ABI numbering */
    static const u32 RelNone = 0;
    static const u32 RelAbs64 = 257;     /* R_AARCH64_ABS64: a pointer in data */
    static const u32 RelGlobDat = 1025;  /* R_AARCH64_GLOB_DAT: a GOT slot */
    static const u32 RelJumpSlot = 1026; /* R_AARCH64_JUMP_SLOT: a PLT's slot */
    static const u32 RelRelative = 1027; /* R_AARCH64_RELATIVE */

    switch (type)
    {
    case RelNone:
        return ModuleReloc::None;
    case RelRelative:
        return ModuleReloc::Relative;
    case RelAbs64:
    case RelGlobDat:
    case RelJumpSlot:
        return ModuleReloc::Symbol;
    default:
        return ModuleReloc::Unsupported;
    }
}

void SetupMemoryTypes()
{
    /* boot.S loads the same MAIR_EL1 on the boot CPU and on every
       secondary, and idx2 is already Normal non-cacheable — the arm64
       equivalent of write-combining. Nothing to do per CPU. */
}

bool IsWriteCombiningAvailable()
{
    return true;
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
