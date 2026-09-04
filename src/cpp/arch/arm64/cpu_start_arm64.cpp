#include "board.h"

#include <kernel/cpu.h>
#include <kernel/stack_probe.h>
#include <kernel/parameters.h>
#include <kernel/trace.h>
#include <kernel/time.h>
#include <mm/page_table.h>
#include <mm/memory_map.h>
#include <lib/stdlib.h>

/* arm64 AP bring-up over PSCI CPU_ON (the x86 twin is the INIT/SIPI
   protocol in arch/x86_64/cpu_start.cpp). Defined as a per-arch TU of
   CpuTable so it keeps private access. */

namespace Kernel
{

namespace
{

const u32 PsciCpuOn = 0xC4000003;

/* PSCI return codes: 0 success. SMCCC allows the callee to clobber
   x4-x17 (QEMU's PSCI happens to preserve them; TF-A does not). */
#define SMCCC_CLOBBERS \
    "x4", "x5", "x6", "x7", "x8", "x9", "x10", "x11", \
    "x12", "x13", "x14", "x15", "x16", "x17", "memory"

long PsciCall(u32 fn, ulong a1, ulong a2, ulong a3)
{
    register ulong x0 asm("x0") = fn;
    register ulong x1 asm("x1") = a1;
    register ulong x2 asm("x2") = a2;
    register ulong x3 asm("x3") = a3;

    if (Board::GetInstance().PsciUseHvc)
        asm volatile("hvc #0" : "+r"(x0) : "r"(x1), "r"(x2), "r"(x3)
            : SMCCC_CLOBBERS);
    else
        asm volatile("smc #0" : "+r"(x0) : "r"(x1), "r"(x2), "r"(x3)
            : SMCCC_CLOBBERS);
    return (long)x0;
}

/* Static boot stacks the APs run on until Cpu::Run switches them to
   their idle-task stacks (the x86 twin: Stack[] in kernel/main.cpp). */
const ulong ApBootStackSize = 4 * Const::PageSize;
char ApBootStack[MaxCpus][4 * Const::PageSize]
    __attribute__((aligned(16)));

/* The MMU-off secondary entry reads these; clean them to the point of
   coherency so the uncached reads see the values. */
void CleanDcacheLine(const void* p)
{
    asm volatile("dc civac, %0" :: "r"(p) : "memory");
}

}

extern "C" char SecondaryEntry[];
extern "C" ulong Arm64ApTtbr1;
extern "C" ulong Arm64ApStackTop[64];
/* sizeof arithmetic rather than Stdlib::ArraySize: that helper is not
   constexpr, so it cannot be used in a static_assert. */
static_assert(sizeof(Arm64ApStackTop) / sizeof(Arm64ApStackTop[0]) == MaxCpus,
    "Arm64ApStackTop (boot.S) must hold one entry per CPU");
static_assert(Board::MaxBoardCpus <= MaxCpus,
    "the FDT CPU table must not outgrow the kernel's CPU cap");

/* boot.S */
extern "C" char BootStack[];
extern "C" char BootStackTop[];

void ReportCpuStacks(Stdlib::Printer& printer, ulong& worstFree)
{
    /* The BSP's boot stack carries all of MainArm64 -- the deepest code on
       this arch -- and the AP stacks carry bring-up until Cpu::Run moves each
       one onto an idle-task stack. None of them has the magic word or the
       one-page tripwire a task stack gets. */
    struct { const char* Name; char* Base; ulong Size; ulong Id; } stacks[MaxCpus + 1];
    ulong count = 0;

    stacks[count].Name = "boot";
    stacks[count].Base = &BootStack[0];
    stacks[count].Size = (ulong)&BootStackTop[0] - (ulong)&BootStack[0];
    stacks[count].Id = 0;
    count++;

    for (ulong i = 0; i < MaxCpus && count < Stdlib::ArraySize(stacks); i++)
    {
        stacks[count].Name = "cpu-boot";
        stacks[count].Base = &ApBootStack[i][0];
        stacks[count].Size = ApBootStackSize;
        stacks[count].Id = i;
        count++;
    }

    for (ulong i = 0; i < count; i++)
    {
        ulong free = StackProbe::Untouched(stacks[i].Base, stacks[i].Size);
        if (free == stacks[i].Size)
            continue;

        printer.Printf("static %u %u %u %s\n", stacks[i].Id,
            stacks[i].Size - free, stacks[i].Size, stacks[i].Name);

        if (free < worstFree)
            worstFree = free;
    }
}

bool CpuTable::StartAll()
{
    auto& board = Board::GetInstance();

    ulong entryPhys = (ulong)&SecondaryEntry[0] - Mm::MemoryMap::KernelSpaceBase;

    Trace(0, "Starting cpus, entry 0x%p", entryPhys);

    Arm64ApTtbr1 = Mm::PageTable::GetInstance().GetRoot();
    for (ulong i = 0; i < Stdlib::ArraySize(Arm64ApStackTop) && i < MaxCpus; i++)
    {
        /* Filled before any AP is on it, so what survives is what it never
           reached. */
        StackProbe::Poison(&ApBootStack[i][0], ApBootStackSize);
        Arm64ApStackTop[i] = (ulong)&ApBootStack[i][ApBootStackSize - 16];
    }

    CleanDcacheLine(&Arm64ApTtbr1);
    for (ulong i = 0; i < Stdlib::ArraySize(Arm64ApStackTop); i++)
        CleanDcacheLine(&Arm64ApStackTop[i]);
    asm volatile("dsb sy" ::: "memory");

    const ulong bspIndex = GetBspIndex();
    const ulong maxCpus = Parameters::GetInstance().GetMaxCpus();
    /* Only the CPUs CPU_ON actually accepted are waited for below: with
       maxcpus=N the rest stay parked in the firmware and never report in. */
    ulong startedMask = 0;
    ulong running = 1; /* the BSP */

    for (ulong index = 0; index < MaxCpus; index++)
    {
        auto& cpu = GetCpu(index);

        if (index == bspIndex || !(cpu.GetState() & Cpu::StateInited))
            continue;

        if (index >= board.CpuCount)
            continue;

        if (maxCpus != 0 && running >= maxCpus)
        {
            Trace(0, "Cpu %u left parked, maxcpus is %u", index, maxCpus);
            continue;
        }

        long err = PsciCall(PsciCpuOn, board.CpuMpidr[index],
            entryPhys, index);
        if (err != 0)
        {
            Trace(0, "Cpu %u CPU_ON failed %d", index, err);
            return false;
        }

        startedMask |= 1UL << index;
        running++;
    }

    /* Poll for the APs to finish startup (mirrors x86): the budget scales
       with the number of them, because each one contends with the others on
       the dmesg lock and the heap on its way up. */
    static const ulong ApTimeoutBaseMs = 500;
    static const ulong ApTimeoutPerCpuMs = 250;
    static const ulong ApPollIntervalMs = 10;

    const ulong ApTimeoutMs = ApTimeoutBaseMs + ApTimeoutPerCpuMs * (running - 1);

    for (ulong waited = 0; waited < ApTimeoutMs; waited += ApPollIntervalMs)
    {
        BusyWait(ApPollIntervalMs * Const::NanoSecsInMs);

        bool allRunning = true;
        for (ulong index = 0; index < MaxCpus; index++)
        {
            if (!(startedMask & (1UL << index)))
                continue;

            if (!(GetCpu(index).GetState() & Cpu::StateRunning))
            {
                allRunning = false;
                break;
            }
        }
        if (allRunning)
            break;
    }

    for (ulong index = 0; index < MaxCpus; index++)
    {
        if (!(startedMask & (1UL << index)))
            continue;

        if (!(GetCpu(index).GetState() & Cpu::StateRunning))
        {
            Trace(0, "Cpu %u still not running after %u ms",
                index, ApTimeoutMs);
            return false;
        }
    }

    Trace(0, "Cpus started, %u running", running);

    return true;
}

}
