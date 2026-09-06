#include <hal/console.h>
#include <hal/power.h>
#include <hal/cpu.h>
#include <hal/mmu.h>
#include <hal/pmu.h>
#include <arch/x86_64/pmu.h>

#include <arch/x86_64/asm.h>
#include <arch/x86_64/cpuid.h>

#include <arch/x86_64/context.h>
#include <lib/stdlib.h>
#include <lib/printer.h>

#include <kernel/trace.h>
#include <kernel/parameters.h>
#include <drivers/serial.h>
#include <drivers/screen.h>
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

namespace
{

/* Set once SetupMemoryTypes() has programmed the PAT; every CPU writes the
   same value, so the racy stores are benign. */
bool WriteCombiningReady;

bool CpuHasPat()
{
    /* CPUID.01H:EDX[16] = PAT */
    u32 eax, ebx, ecx, edx;

    asm volatile("cpuid"
        : "=a"(eax), "=b"(ebx), "=c"(ecx), "=d"(edx)
        : "a"(1U), "c"(0U));

    return (edx & (1U << 16)) != 0;
}

}

void SetupMemoryTypes()
{
    static const u32 PatMsr = 0x277;
    static const u32 WcEntry = 4;      /* PTE bit PAT=1, PCD=0, PWT=0 */
    static const u64 PatTypeWc = 0x01;
    static const u64 PatEntryMask = 0xFF;

    if (!CpuHasPat())
    {
        Trace(0, "PAT: not supported, MMIO stays uncached");
        return;
    }

    u64 pat = ReadMsr(PatMsr);
    pat &= ~(PatEntryMask << (WcEntry * 8));
    pat |= PatTypeWc << (WcEntry * 8);

    /* Changing a memory type wants the caches flushed and the TLB clean
       (SDM 11.11.8). Interrupts are still off here on both the BSP and the
       APs, and entry 4 is not used by any existing mapping. */
    asm volatile("wbinvd" ::: "memory");
    WriteMsr(PatMsr, pat);
    asm volatile("wbinvd" ::: "memory");
    TlbFlushAll();

    WriteCombiningReady = true;

    Trace(0, "PAT: 0x%p, entry %u = WC", (ulong)pat, (ulong)WcEntry);
}

bool IsWriteCombiningAvailable()
{
    return WriteCombiningReady;
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

namespace
{

void PrintFeature(Stdlib::Printer& con, const char* name, bool present,
    const char* why)
{
    con.Printf("  %s %s  %s\n", present ? "yes" : "NO ", name, why);
}

}

void PrintCpuInfo(Stdlib::Printer& con)
{
    static const u32 ExtendedLeafBase = 0x80000000;

    CpuidResult r0 = Cpuid(0);

    /* Vendor string is EBX:EDX:ECX, in that order. */
    char vendor[13];
    Stdlib::MemCpy(&vendor[0], &r0.Ebx, 4);
    Stdlib::MemCpy(&vendor[4], &r0.Edx, 4);
    Stdlib::MemCpy(&vendor[8], &r0.Ecx, 4);
    vendor[12] = '\0';

    u32 maxExtended = Cpuid(ExtendedLeafBase).Eax;

    char brand[49];
    brand[0] = '\0';
    if (maxExtended >= ExtendedLeafBase + 4)
    {
        for (u32 i = 0; i < 3; i++)
        {
            CpuidResult b = Cpuid(ExtendedLeafBase + 2 + i);
            Stdlib::MemCpy(&brand[i * 16 + 0], &b.Eax, 4);
            Stdlib::MemCpy(&brand[i * 16 + 4], &b.Ebx, 4);
            Stdlib::MemCpy(&brand[i * 16 + 8], &b.Ecx, 4);
            Stdlib::MemCpy(&brand[i * 16 + 12], &b.Edx, 4);
        }
        brand[48] = '\0';
    }

    CpuidResult r1 = Cpuid(1);

    /* Family and model carry their extended halves above 0xF / 0x6
       respectively (SDM 3-217). */
    ulong family = (r1.Eax >> 8) & 0xF;
    ulong model = (r1.Eax >> 4) & 0xF;
    ulong stepping = r1.Eax & 0xF;
    if (family == 0xF)
        family = family + ((r1.Eax >> 20) & 0xFF);
    if (family == 0x6 || family == 0xF)
        model = model + (((r1.Eax >> 16) & 0xF) << 4);

    con.Printf("vendor %s\n", vendor);
    if (brand[0] != '\0')
        con.Printf("model  %s\n", brand);
    con.Printf("family %u model %u stepping %u, max leaf 0x%p ext 0x%p\n",
        family, model, stepping, (ulong)r0.Eax, (ulong)maxExtended);

    CpuidResult ext1 = (maxExtended >= ExtendedLeafBase + 1)
        ? Cpuid(ExtendedLeafBase + 1) : CpuidResult{0, 0, 0, 0};
    CpuidResult ext7 = (maxExtended >= ExtendedLeafBase + 7)
        ? Cpuid(ExtendedLeafBase + 7) : CpuidResult{0, 0, 0, 0};
    CpuidResult r6 = (r0.Eax >= 6) ? Cpuid(6) : CpuidResult{0, 0, 0, 0};

    con.Printf("features the kernel depends on:\n");
    PrintFeature(con, "pdpe1gb ", (ext1.Edx & (1u << 26)) != 0,
        "1GiB pages: RAM above 4GiB");
    PrintFeature(con, "nx      ", (ext1.Edx & (1u << 20)) != 0,
        "no-execute: W^X");
    PrintFeature(con, "pat     ", (r1.Edx & (1u << 16)) != 0,
        "write-combining framebuffer");
    PrintFeature(con, "invtsc  ", (ext7.Edx & (1u << 8)) != 0,
        "invariant tsc: timekeeping");
    PrintFeature(con, "arat    ", (r6.Eax & (1u << 2)) != 0,
        "always-running apic timer: the per-cpu tick");
    PrintFeature(con, "x2apic  ", (r1.Ecx & (1u << 21)) != 0,
        "firmware may hand off in this mode");
    PrintFeature(con, "tscdl   ", (r1.Ecx & (1u << 24)) != 0,
        "tsc-deadline timer (unused)");
    PrintFeature(con, "cx16    ", (r1.Ecx & (1u << 13)) != 0,
        "cmpxchg16b (unused)");
    PrintFeature(con, "hyperv  ", (r1.Ecx & (1u << 31)) != 0,
        "running under a hypervisor");

    /* What the profiler can sample with. The two vendors enumerate entirely
       different things here, and CPUID.0AH on an AMD part reads back as
       zeroes -- printing it there would say "no counters" about a machine
       that has six. */
    static const u32 AmdVendorEbx = 0x68747541;   /* "Auth" */
    if (r0.Ebx == AmdVendorEbx)
    {
        /* Fn8000_0022 is PerfMonV2: global control and status, Zen 4 and
           later, and the only place AMD states a counter count. Without it
           the count follows Fn8000_0001_ECX[PerfCtrExtCore]: six with, the
           four legacy ones without. Width is 48 bits on every AMD part and
           is enumerated nowhere. */
        CpuidResult r22 = (maxExtended >= ExtendedLeafBase + 0x22)
            ? Cpuid(ExtendedLeafBase + 0x22) : CpuidResult{0, 0, 0, 0};
        bool perfMonV2 = (r22.Eax & 1u) != 0;
        bool extCore = (ext1.Ecx & (1u << 23)) != 0;

        con.Printf("amd perfmon: %u core counters 48 bits, perfctr-core %s, "
            "perfmon v2 %s\n",
            perfMonV2 ? (ulong)(r22.Ebx & 0xF) : (extCore ? 6UL : 4UL),
            extCore ? "yes" : "NO", perfMonV2 ? "yes" : "no");
    }
    else
    {
        /* CPUID.0AH: architectural performance monitoring. Version 0 means
           the counters are not there to program -- which is the case under
           TCG, and is what decides whether this machine can sample faster
           than its tick. */
        CpuidResult r10 = (r0.Eax >= 0xA) ? Cpuid(0xA) : CpuidResult{0, 0, 0, 0};
        ulong perfVersion = r10.Eax & 0xFF;
        con.Printf("arch perfmon: version %u, %u general counters %u bits, "
            "%u fixed counters %u bits\n",
            perfVersion, (r10.Eax >> 8) & 0xFF, (r10.Eax >> 16) & 0xFF,
            (ulong)(r10.Edx & 0x1F), (ulong)((r10.Edx >> 5) & 0xFF));
    }

    con.Printf("profile sampling: %s\n",
        PmuAvailable() ? PmuName() : "tick only");

    /* Only ever non-zero on AMD, and only after a profiling run. Printed
       here because the NMI that produced it could not safely say so
       itself. */
    ulong spurious = Kernel::Pmu::SpuriousNmiCount();
    if (spurious != 0)
        con.Printf("  %u late performance-counter nmis absorbed\n", spurious);
}

bool PmuAvailable()
{
    return Kernel::Pmu::Available();
}

bool PmuStart()
{
    return Kernel::Pmu::Start(Kernel::Pmu::DefaultPeriod);
}

void PmuStop()
{
    Kernel::Pmu::Stop();
}

const char* PmuName()
{
    return Kernel::Pmu::Name();
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
        Kernel::Screen::PrintString(s);
    if (UseSerial())
        Kernel::Serial::GetInstance().PrintString(s);
}

void ConsoleOutBackspace()
{
    if (UseVga())
        Kernel::Screen::Backspace();
    if (UseSerial())
        Kernel::Serial::GetInstance().Backspace();
}

void ConsoleOutClear()
{
    if (UseVga())
        Kernel::Screen::Cls();
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

    /* The boot trace goes out the serial port and is mirrored on the screen
       only on request -- redrawing glyphs for every line costs a full
       repaint per scrolled row. A machine with no UART has nowhere else to
       put it, so there the mirror is the default rather than the option. */
    if ((Kernel::Parameters::GetInstance().IsTraceVga() ||
         !Kernel::Serial::GetInstance().IsPresent()) &&
        Kernel::Screen::IsReady())
    {
        Kernel::Screen::PrintString(msg);
    }
}

void ConsolePanicWrite(const char *msg)
{
    Kernel::Serial::GetInstance().PanicPrintString(msg);
    if (Kernel::Screen::IsReady())
        Kernel::Screen::PanicPrintString(msg);
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
