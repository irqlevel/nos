#include <kernel/cpu.h>
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

bool CpuTable::StartAll()
{
    ulong startupCode = (ulong)ApStart16;

    Trace(0, "Starting cpus, startupCode 0x%p", startupCode);

    if (startupCode & (Const::PageSize - 1))
        return false;

    if (startupCode >= 0x100000)
        return false;

    {
        Stdlib::AutoLock lock(Lock);
        for (ulong index = 0; index < Stdlib::ArraySize(CpuArray); index++)
        {
            if (index != GetBspIndexLockHeld() && (CpuArray[index].GetState() & Cpu::StateInited))
            {
                Lapic::SendInit(index);
            }
        }
    }

    BusyWait(10 * Const::NanoSecsInMs); /* 10ms after INIT */

    /*
     * Intel MP spec: send two SIPIs, 200µs apart.
     * The first SIPI can be lost on some hardware/hypervisors.
     */
    static const ulong SipiRetries = 2;
    static const ulong SipiDelayUs = 200;

    for (ulong sipi = 0; sipi < SipiRetries; sipi++)
    {
        {
            Stdlib::AutoLock lock(Lock);
            for (ulong index = 0; index < Stdlib::ArraySize(CpuArray); index++)
            {
                if (index != GetBspIndexLockHeld() && (CpuArray[index].GetState() & Cpu::StateInited))
                {
                    if (!(CpuArray[index].GetState() & Cpu::StateRunning))
                        Lapic::SendStartup(index, startupCode >> Const::PageShift);
                }
            }
        }

        BusyWait(SipiDelayUs * Const::NanoSecsInUsec); /* 200µs */
    }

    /* Poll for APs to finish startup, up to 500ms */
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
                if (index != GetBspIndexLockHeld() && (CpuArray[index].GetState() & Cpu::StateInited))
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
            if (index != GetBspIndexLockHeld() && (CpuArray[index].GetState() & Cpu::StateInited))
            {
                if (!(CpuArray[index].GetState() & Cpu::StateRunning))
                {
                    Trace(0, "Cpu %u still not running after %u ms", index, ApTimeoutMs);
                    return false;
                }
            }
        }
    }

    Trace(0, "Cpus started");

    return true;
}

}
