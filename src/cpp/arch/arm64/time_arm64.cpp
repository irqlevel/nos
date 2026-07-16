#include <kernel/time.h>
#include <kernel/trace.h>

/* arm64 implementation of the kernel/time.h API over the ARM generic
   timer: CNTVCT_EL0 at CNTFRQ_EL0 Hz, architecturally specified — no
   calibration needed (x86 twin: kernel/time.cpp + arch/x86_64/tsc.cpp).
   Wall clock arrives with the PL031 driver (milestone M3). */

namespace Kernel
{

namespace
{

u64 BootCount;
u64 FreqHz;

u64 ReadCntvct()
{
    u64 cnt;
    asm volatile("isb; mrs %0, cntvct_el0" : "=r"(cnt));
    return cnt;
}

u64 ReadCntfrq()
{
    u64 freq;
    asm volatile("mrs %0, cntfrq_el0" : "=r"(freq));
    return freq;
}

u64 CountToNs(u64 count)
{
    /* Split to avoid 128-bit division (no compiler-rt in the kernel):
       remainder * 1e9 stays < 2^63 for any sane counter frequency. */
    return (count / FreqHz) * Const::NanoSecsInSec +
           ((count % FreqHz) * Const::NanoSecsInSec) / FreqHz;
}

}

void TimeInit()
{
    if (FreqHz == 0)
    {
        /* Not yet self-armed by an early GetBootTime call */
        FreqHz = ReadCntfrq();
        BootCount = ReadCntvct();
    }
    Trace(0, "TimeInit: generic timer freq %u Hz", FreqHz);
}

Stdlib::Time GetBootTime()
{
    if (FreqHz == 0)
    {
        /* Callable before TimeInit (locks trace boot time); self-arm */
        FreqHz = ReadCntfrq();
        BootCount = ReadCntvct();
    }

    Stdlib::Time time;
    time.NanoSecs = CountToNs(ReadCntvct() - BootCount);
    return time;
}

void BusyWait(ulong nanoSecs)
{
    if (FreqHz == 0)
        GetBootTime();

    u64 target = ReadCntvct() +
        (nanoSecs / Const::NanoSecsInSec) * FreqHz +
        ((nanoSecs % Const::NanoSecsInSec) * FreqHz) / Const::NanoSecsInSec;
    while (ReadCntvct() < target)
    {
        asm volatile("yield");
    }
}

ulong GetWallTimeSecs()
{
    /* PL031 RTC lands in M3; boot-relative until then */
    return GetBootTime().NanoSecs / Const::NanoSecsInSec;
}

}
