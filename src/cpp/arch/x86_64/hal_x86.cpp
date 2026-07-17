#include <hal/console.h>
#include <hal/power.h>
#include <hal/cpu.h>
#include <hal/mmu.h>

#include <arch/x86_64/asm.h>

#include <arch/x86_64/context.h>
#include <lib/stdlib.h>
#include <lib/printer.h>

#include <kernel/trace.h>
#include <kernel/parameters.h>
#include <drivers/serial.h>
#include <drivers/vga.h>
#include <drivers/acpi.h>

namespace Hal
{

void EnableWxSupport()
{
    /* EFER (MSR 0xC0000080) bit 11 = NXE: honor the NX bit in PTEs. */
    static const u32 EferMsr = 0xC0000080;
    static const u64 EferNxe = 1ULL << 11;
    u64 efer = ReadMsr(EferMsr);
    if (!(efer & EferNxe))
        WriteMsr(EferMsr, efer | EferNxe);

    /* CR0.WP (bit 16): without it, ring-0 writes ignore the read-only PTE
       bit, so the kernel could still write .text. Enforce it for W^X. */
    asm volatile(
        "mov %%cr0, %%rax\n\t"
        "or $0x10000, %%rax\n\t"
        "mov %%rax, %%cr0\n\t"
        ::: "rax", "memory");
}

ulong MmioPremappedVa(ulong physAddr, ulong sizeBytes)
{
    (void)physAddr;
    (void)sizeBytes;
    return 0;
}

ulong BuildTaskFrame(ulong stackTop, ulong entry, ulong arg)
{
    ulong* rsp = (ulong *)stackTop;
    *(--rsp) = entry; /* return address SwitchContext's ret pops */
    Kernel::Context* regs = (Kernel::Context*)((ulong)rsp - sizeof(*regs));
    Stdlib::MemSet(regs, 0, sizeof(*regs));
    regs->Rdi = arg;         /* 1st argument for the entry function */
    regs->Rflags = (1 << 9); /* IF */
    return (ulong)regs;
}

void PrintCpuState(Stdlib::Printer& con)
{
    con.Printf("ss 0x%p cs 0x%p ds 0x%p gs 0x%p fs 0x%p es 0x%p",
        (ulong)GetSs(), (ulong)GetCs(), (ulong)GetDs(),
        (ulong)GetGs(), (ulong)GetFs(), (ulong)GetEs());

    con.Printf("rflags 0x%p rsp 0x%p rip 0x%p\n",
        GetRflags(), GetRsp(), GetRip());

    con.Printf("cr0 0x%p cr2 0x%p cr3 0x%p cr4 0x%p",
        GetCr0(), GetCr2(), GetCr3(), GetCr4());
}

ulong TaskSavedFramePointer(ulong savedSp)
{
    return ((Kernel::Context*)savedSp)->Rbp;
}

void RunOnStack(ulong stackTop, void (*fn)(void*), void* ctx)
{
    asm volatile(
        "movq %1, %%rsp\n\t"
        "callq *%0\n\t"
        :: "a"(fn), "r"(stackTop), "D"(ctx)
        : "memory");
    Trace(0, "RunOnStack: fn returned");
    while (1)
        Hlt();
}

namespace
{

bool UseVga()
{
    return !Kernel::Parameters::GetInstance().IsConsoleSerial();
}

bool UseSerial()
{
    return !Kernel::Parameters::GetInstance().IsConsoleVga();
}

}

void ConsoleOut(const char *s)
{
    if (UseVga())
        Kernel::VgaTerm::GetInstance().PrintString(s);
    if (UseSerial())
        Kernel::Serial::GetInstance().PrintString(s);
}

void ConsoleOutBackspace()
{
    if (UseVga())
        Kernel::VgaTerm::GetInstance().Backspace();
    if (UseSerial())
        Kernel::Serial::GetInstance().Backspace();
}

void ConsoleOutClear()
{
    if (UseVga())
        Kernel::VgaTerm::GetInstance().Cls();
    if (UseSerial())
    {
        /* ANSI escape: clear screen and move cursor home */
        Kernel::Serial::GetInstance().PrintString("\033[2J\033[H");
    }
}

void ConsoleWrite(const char *msg)
{
    Kernel::Serial::GetInstance().PrintString(msg);
    Kernel::Serial::GetInstance().Flush();

    if (Kernel::Parameters::GetInstance().IsTraceVga() && Kernel::VgaTerm::IsReady())
    {
        Kernel::VgaTerm::GetInstance().PrintString(msg);
    }
}

void ConsolePanicWrite(const char *msg)
{
    Kernel::Serial::GetInstance().PanicPrintString(msg);
    if (Kernel::VgaTerm::IsReady())
        Kernel::VgaTerm::GetInstance().PanicPrintString(msg);
}

void PowerOff()
{
    Trace(0, "ACPI shutdown");

    /* Try PM1a_CNT from FADT with SLP_TYP=5 (S5) | SLP_EN */
    ulong pm1a = Kernel::Acpi::GetInstance().GetPm1aCntPort();
    if (pm1a != 0)
    {
        Outw((u16)pm1a, (5 << 10) | (1 << 13));
        /* Brief busy-wait for the hardware to respond */
        for (volatile int i = 0; i < 1000000; i = i + 1) {}
    }

    /* QEMU/KVM fallback: PM1a_CNT port 0x604, SLP_TYP=0 for S5 */
    Outw(0x604, (1 << 13));

    /* QEMU debug exit device fallback */
    Outb(0xf4, 0x0);

    while (1) Hlt();
}

void Reset()
{
    Trace(0, "Reboot");

    /* Try ACPI FADT RESET_REG first */
    auto& acpi = Kernel::Acpi::GetInstance();
    if (acpi.HasResetReg())
    {
        Outb((u16)acpi.GetResetRegPort(), acpi.GetResetValue());
        for (volatile int i = 0; i < 1000000; i = i + 1) {}
    }

    /* Keyboard controller reset (pulse CPU reset line) */
    Outb(0x64, 0xFE);

    /* Fallback: PCI reset register */
    Outb(0xCF9, 0x06);

    while (1) Hlt();
}

}
