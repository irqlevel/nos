#include "board.h"

#include <kernel/cpu.h>
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

/* PSCI return codes: 0 success */
long PsciCall(u32 fn, ulong a1, ulong a2, ulong a3)
{
    register ulong x0 asm("x0") = fn;
    register ulong x1 asm("x1") = a1;
    register ulong x2 asm("x2") = a2;
    register ulong x3 asm("x3") = a3;

    if (Board::GetInstance().PsciUseHvc)
        asm volatile("hvc #0" : "+r"(x0) : "r"(x1), "r"(x2), "r"(x3) : "memory");
    else
        asm volatile("smc #0" : "+r"(x0) : "r"(x1), "r"(x2), "r"(x3) : "memory");
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
extern "C" ulong Arm64ApStackTop[8];

bool CpuTable::StartAll()
{
    auto& board = Board::GetInstance();

    ulong entryPhys = (ulong)&SecondaryEntry[0] - Mm::MemoryMap::KernelSpaceBase;

    Trace(0, "Starting cpus, entry 0x%p", entryPhys);

    Arm64ApTtbr1 = Mm::PageTable::GetInstance().GetRoot();
    for (ulong i = 0; i < Stdlib::ArraySize(Arm64ApStackTop) && i < MaxCpus; i++)
        Arm64ApStackTop[i] = (ulong)&ApBootStack[i][ApBootStackSize - 16];

    CleanDcacheLine(&Arm64ApTtbr1);
    for (ulong i = 0; i < Stdlib::ArraySize(Arm64ApStackTop); i++)
        CleanDcacheLine(&Arm64ApStackTop[i]);
    asm volatile("dsb sy" ::: "memory");

    {
        Stdlib::AutoLock lock(Lock);
        for (ulong index = 0; index < Stdlib::ArraySize(CpuArray); index++)
        {
            if (index == GetBspIndexLockHeld() ||
                !(CpuArray[index].GetState() & Cpu::StateInited))
                continue;

            if (index >= board.CpuCount)
                continue;

            long err = PsciCall(PsciCpuOn, board.CpuMpidr[index],
                entryPhys, index);
            if (err != 0)
            {
                Trace(0, "Cpu %u CPU_ON failed %d", index, err);
                return false;
            }
        }
    }

    /* Poll for APs to finish startup, up to 500ms (mirrors x86) */
    static const ulong ApTimeoutMs = 500;
    static const ulong ApPollIntervalMs = 10;

    for (ulong waited = 0; waited < ApTimeoutMs; waited += ApPollIntervalMs)
    {
        BusyWait(ApPollIntervalMs * Const::NanoSecsInMs);

        bool allRunning = true;
        {
            Stdlib::AutoLock lock(Lock);
            for (ulong index = 0; index < Stdlib::ArraySize(CpuArray); index++)
            {
                if (index != GetBspIndexLockHeld() &&
                    (CpuArray[index].GetState() & Cpu::StateInited))
                {
                    if (!(CpuArray[index].GetState() & Cpu::StateRunning))
                    {
                        allRunning = false;
                        break;
                    }
                }
            }
        }
        if (allRunning)
            break;
    }

    {
        Stdlib::AutoLock lock(Lock);
        for (ulong index = 0; index < Stdlib::ArraySize(CpuArray); index++)
        {
            if (index != GetBspIndexLockHeld() &&
                (CpuArray[index].GetState() & Cpu::StateInited))
            {
                if (!(CpuArray[index].GetState() & Cpu::StateRunning))
                {
                    Trace(0, "Cpu %u still not running after %u ms",
                        index, ApTimeoutMs);
                    return false;
                }
            }
        }
    }

    Trace(0, "Cpus started");

    return true;
}

}
