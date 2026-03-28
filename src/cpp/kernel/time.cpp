#include "time.h"
#include "tsc.h"
#include "trace.h"

#include <drivers/pit.h>
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

void TimeInit()
{
    auto& tsc = Tsc::GetInstance();

    /* Always calibrate TSC if possible */
    if (tsc.Calibrate())
    {
        if (tsc.IsInvariant())
            ActiveSource = ClockTsc;
    }
    else
    {
        Trace(0, "Time: TSC calibration failed");
    }

    /* Try kvmclock (takes priority over TSC for GetBootTime) */
    if (tsc.SetupKvmClock())
    {
        ActiveSource = ClockKvmClock;
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

    /* Read RTC wall clock */
    RtcTime rtc;
    if (Rtc::GetInstance().ReadTime(rtc))
    {
        RtcEpochSecs = Rtc::ToUnixEpoch(rtc);
        Trace(0, "Time: RTC epoch %u", RtcEpochSecs);
    }
}

Stdlib::Time GetBootTime()
{
    switch (ActiveSource)
    {
    case ClockKvmClock:
    case ClockTsc:
    {
        Stdlib::Time t = Tsc::GetInstance().GetTime();
        if (t.GetValue() != 0)
            return t;
        /* Fallthrough to PIT if TSC returns 0 (shouldn't happen) */
        break;
    }
    default:
        break;
    }

    return Pit::GetInstance().GetTime();
}

ulong GetWallTimeSecs()
{
    return RtcEpochSecs + GetBootTime().GetSecs();
}

}
