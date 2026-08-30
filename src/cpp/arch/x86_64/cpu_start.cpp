#include <kernel/cpu.h>
#include <kernel/parameters.h>
#include <kernel/time.h>
#include <kernel/trace.h>
#include <arch/x86_64/lapic.h>
#include <arch/x86_64/boot64.h>

#include <lib/stdlib.h>

namespace Kernel
{

/* The x86 MP startup protocol (INIT + 2x SIPI + poll) for CpuTable.
   Defined in an arch TU so another architecture can supply its own
   CpuTable::StartAll (e.g. PSCI CPU_ON) with full private access. */

namespace
{

/* Bring one AP up and wait for it to report in.

   One CPU at a time, rather than INIT-ing all of them and then waiting for
   the crowd: the trace line below names the CPU that is being poked, so a
   machine that dies during bring-up says which one it choked on, and the APs
   no longer race each other through the dmesg lock, the heap and the shared
   GDT on their way up. The cost is the 10ms INIT delay per CPU -- the polling
   ends as soon as that one AP reports running. */
bool StartCpu(Cpu& cpu, ulong index, ulong startupVector)
{
    static const ulong InitDelayMs = 10;   /* 10ms after INIT */
    static const ulong SipiRetries = 2;    /* the first SIPI can be lost */
    static const ulong SipiDelayUs = 200;  /* Intel MP spec: 200us apart */
    static const ulong ApTimeoutMs = 1000;
    static const ulong ApPollIntervalMs = 1;

    Trace(0, "Cpu %u: INIT+SIPI vector 0x%p", index, startupVector);

    Lapic::SendInit(index);
    BusyWait(InitDelayMs * Const::NanoSecsInMs);

    for (ulong sipi = 0; sipi < SipiRetries; sipi++)
    {
        if (cpu.GetState() & Cpu::StateRunning)
            return true;

        Lapic::SendStartup(index, startupVector);
        BusyWait(SipiDelayUs * Const::NanoSecsInUsec);
    }

    for (ulong waited = 0; waited < ApTimeoutMs; waited += ApPollIntervalMs)
    {
        if (cpu.GetState() & Cpu::StateRunning)
            return true;

        BusyWait(ApPollIntervalMs * Const::NanoSecsInMs);
    }

    Trace(0, "Cpu %u still not running after %u ms", index, ApTimeoutMs);

    return false;
}

}

bool CpuTable::StartAll()
{
    ulong startupCode = (ulong)ApStart16;

    Trace(0, "Starting cpus, startupCode 0x%p", startupCode);

    if (startupCode & (Const::PageSize - 1))
        return false;

    if (startupCode >= 0x100000)
        return false;

    const ulong bspIndex = GetBspIndex();
    const ulong maxCpus = Parameters::GetInstance().GetMaxCpus();
    ulong running = 1; /* the BSP */

    for (ulong index = 0; index < MaxCpus; index++)
    {
        auto& cpu = GetCpu(index);

        if (index == bspIndex || !(cpu.GetState() & Cpu::StateInited))
            continue;

        if (maxCpus != 0 && running >= maxCpus)
        {
            Trace(0, "Cpu %u left parked, maxcpus is %u", index, maxCpus);
            continue;
        }

        if (!StartCpu(cpu, index, startupCode >> Const::PageShift))
            return false;

        running++;
    }

    Trace(0, "Cpus started, %u running", running);

    return true;
}

}
