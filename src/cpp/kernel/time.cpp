#include "time.h"
#include <arch/x86_64/tsc.h>
#include "trace.h"
#include <hal/cpu.h>

#include <drivers/pit.h>
#include <drivers/hpet.h>
#include <drivers/rtc.h>

namespace Kernel
{

enum ClockSource
{
    ClockPit = 0,
    ClockTsc,
    ClockKvmClock,
};

static ClockSource ActiveSource = ClockPit;
static ulong RtcEpochSecs = 0;

/* Added to the active source's reading so GetBootTime() stays one
   continuous timeline across a source switch (see SetClockSource).
   The HPET/PIT fallback below defines that timeline and takes no
   offset. */
static long ClockOffsetNs = 0;

/* The timeline GetBootTime() reports: the HPET the kernel reset in
   Hpet::Setup(), or the PIT when there is no HPET. It runs from before
   the first GetBootTime() call to the last, so it is the one clock every
   other source is aligned onto and it never takes an offset. */
static Stdlib::Time ReadBaseClock()
{
    if (Hpet::GetInstance().IsAvailable())
        return Hpet::GetInstance().GetTime();

    return Pit::GetInstance().GetTime();
}

/* Reads the active source, or the base clock when that source has
   nothing to say. *primary reports which one answered, because only the
   active source needs the offset. */
static Stdlib::Time ReadClockSource(bool* primary)
{
    *primary = true;

    switch (ActiveSource)
    {
    case ClockKvmClock:
    case ClockTsc:
    {
        /* Ask for the source this offset was computed against, not for
           Tsc::GetTime()'s "best" one: that one silently becomes kvmclock
           the moment SetupKvmClock() arms it, which would move a ClockTsc
           reading onto a raw clock with a different zero. */
        Stdlib::Time t = (ActiveSource == ClockKvmClock)
            ? Tsc::GetInstance().GetTime()
            : Tsc::GetInstance().TscTime();
        if (t.GetValue() != 0)
            return t;
        /* Fallthrough to the base clock if TSC returns 0 (shouldn't happen) */
        break;
    }
    default:
        break;
    }

    *primary = false;

    return ReadBaseClock();
}

/* Point GetBootTime() at another source without moving the clock.

   Every source has its own zero: the HPET main counter is reset in
   Hpet::Setup(), the TSC baseline is taken here in TimeInit(), and
   kvmclock counts from VM start. Switching to one raw therefore steps
   the reported time -- back by everything the kernel did between the
   two zeros (the self test alone runs tens of seconds), or forward
   onto a clock that was already running before the kernel was.

   Whatever holds an absolute deadline across the step then waits it
   out. The 8042 timer that decodes scancodes is armed from
   Interrupt::Register, before TimeInit, at now + 10ms: step the clock
   back and nothing decodes a keystroke until the new clock passes that
   mark, so keys pile up in the driver's ring buffer and all land at
   once, half a minute later. Stepping forward instead makes every lock
   the watchdog is timing look held for the size of the step.

   The two readings are taken from the new source and from the base
   clock, never from GetBootTime(): enabling a source can change what the
   *current* source reads before ActiveSource moves -- SetupKvmClock()
   flips Tsc::GetTime() over to kvmclock -- so a "before" taken through
   GetBootTime() would already be on the new raw clock carrying the old
   offset. That measures a zero step, keeps the stale offset and doubles
   the reported boot time.

   IRQs are off because ProcessTimers reads the clock from the tick IPI
   on this CPU; the APs are not started yet, so there is no other
   reader. */
static void SetClockSource(ClockSource source)
{
    ulong flags = Hal::IrqSave();

    ActiveSource = source;
    ClockOffsetNs = 0;

    bool primary = false;
    ulong raw = ReadClockSource(&primary).GetValue();
    if (primary)
        ClockOffsetNs = (long)ReadBaseClock().GetValue() - (long)raw;

    Hal::IrqRestore(flags);
}

void TimeInit()
{
    auto& tsc = Tsc::GetInstance();

    /* Always calibrate TSC if possible */
    if (tsc.Calibrate())
    {
        if (tsc.IsInvariant())
            SetClockSource(ClockTsc);
    }
    else
    {
        Trace(0, "Time: TSC calibration failed");
    }

    /* Try kvmclock (takes priority over TSC for GetBootTime) */
    if (tsc.SetupKvmClock())
    {
        SetClockSource(ClockKvmClock);
        Trace(0, "Time: using kvmclock");
    }
    else if (ActiveSource == ClockTsc)
    {
        Trace(0, "Time: using calibrated TSC");
    }
    else
    {
        Trace(0, "Time: using PIT fallback");
    }

    /* Read RTC wall clock. Store the epoch of boot time zero, not of
       now: GetBootTime() already counts the stretch before TimeInit, and
       GetWallTimeSecs adds the two. */
    RtcTime rtc;
    if (Rtc::GetInstance().ReadTime(rtc))
    {
        RtcEpochSecs = Rtc::ToUnixEpoch(rtc) - GetBootTime().GetSecs();
        Trace(0, "Time: RTC epoch %u", RtcEpochSecs);
    }

    /* Counts from the kernel's own start, not from this line: the
       restart-at-zero this replaces hid how long the self test and the
       device probe really take. */
    Trace(0, "Time: boot time %u ns", GetBootTime().GetValue());
}

Stdlib::Time GetBootTime()
{
    bool primary = false;
    ulong raw = ReadClockSource(&primary).GetValue();

    if (!primary)
        return Stdlib::Time(raw);

    long offset = ClockOffsetNs;

    if (offset >= 0)
        return Stdlib::Time(raw + (ulong)offset);

    ulong back = (ulong)(-offset);

    return Stdlib::Time((raw > back) ? (raw - back) : 0);
}

void BusyWait(ulong nanoSecs)
{
    Stdlib::Time expired = GetBootTime() + Stdlib::Time(nanoSecs);
    while (GetBootTime() < expired)
    {
        Pause();
    }
}

ulong GetWallTimeSecs()
{
    return RtcEpochSecs + GetBootTime().GetSecs();
}

}
