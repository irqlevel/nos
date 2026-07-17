#include <kernel/time.h>
#include <kernel/trace.h>
#include <mm/memory_map.h>

#include "board.h"

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
ulong BootEpochSecs;

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

    /* PL031 RTC: seconds since the Unix epoch (QEMU virt), through the
       premapped device GiB. The x86 twin reads the CMOS RTC. */
    ulong pl031 = Board::GetInstance().Pl031Base;
    if (pl031 != 0)
    {
        u32 now = *reinterpret_cast<volatile u32*>(
            Mm::MemoryMap::KernelSpaceBase + pl031);
        ulong bootedSecs = CountToNs(ReadCntvct() - BootCount) /
            Const::NanoSecsInSec;
        BootEpochSecs = now - bootedSecs;
        Trace(0, "TimeInit: rtc epoch %u", (ulong)now);
    }
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
    return BootEpochSecs + GetBootTime().NanoSecs / Const::NanoSecsInSec;
}

}
