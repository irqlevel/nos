#include "rtc.h"

#include <kernel/asm.h>
#include <kernel/trace.h>

namespace Kernel
{

Rtc::Rtc()
{
}

Rtc::~Rtc()
{
}

u8 Rtc::ReadRegister(u8 reg)
{
    Outb(IndexPort, reg);
    return Inb(DataPort);
}

u8 Rtc::BcdToBin(u8 bcd)
{
    return (bcd & 0x0F) + ((bcd >> 4) * 10);
}

bool Rtc::ReadTime(RtcTime& t)
{
    /* Wait until update-in-progress clears */
    ulong timeout = UipTimeout;
    while ((ReadRegister(RegStatusA) & StatusAUip) && timeout > 0)
        timeout--;

    /* Read first snapshot */
    u8 sec1 = ReadRegister(RegSeconds);
    u8 min1 = ReadRegister(RegMinutes);
    u8 hr1  = ReadRegister(RegHours);
    u8 day1 = ReadRegister(RegDay);
    u8 mon1 = ReadRegister(RegMonth);
    u8 yr1  = ReadRegister(RegYear);
    u8 cen1 = ReadRegister(RegCentury);

    /* Read second snapshot and compare to avoid mid-update inconsistency */
    u8 sec2, min2, hr2, day2, mon2, yr2, cen2;
    do {
        sec2 = sec1; min2 = min1; hr2 = hr1;
        day2 = day1; mon2 = mon1; yr2 = yr1; cen2 = cen1;

        /* Wait for update-in-progress to clear again */
        timeout = UipTimeout;
        while ((ReadRegister(RegStatusA) & StatusAUip) && timeout > 0)
            timeout--;

        sec1 = ReadRegister(RegSeconds);
        min1 = ReadRegister(RegMinutes);
        hr1  = ReadRegister(RegHours);
        day1 = ReadRegister(RegDay);
        mon1 = ReadRegister(RegMonth);
        yr1  = ReadRegister(RegYear);
        cen1 = ReadRegister(RegCentury);
    } while (sec1 != sec2 || min1 != min2 || hr1 != hr2 ||
             day1 != day2 || mon1 != mon2 || yr1 != yr2 || cen1 != cen2);

    u8 statusB = ReadRegister(RegStatusB);
    bool binaryMode = (statusB & StatusBBinary) != 0;
    bool mode24h    = (statusB & StatusB24Hour) != 0;

    if (!binaryMode)
    {
        sec1 = BcdToBin(sec1);
        min1 = BcdToBin(min1);
        hr1  = BcdToBin(hr1 & HourMask) | (hr1 & HourPmBit); /* preserve PM bit */
        day1 = BcdToBin(day1);
        mon1 = BcdToBin(mon1);
        yr1  = BcdToBin(yr1);
        cen1 = BcdToBin(cen1);
    }

    if (!mode24h)
    {
        if (hr1 & HourPmBit)
            hr1 = ((hr1 & HourMask) % HoursPerHalfDay) + HoursPerHalfDay;
        else if ((hr1 & HourMask) == HoursPerHalfDay)
            hr1 = 0; /* 12 AM = midnight */
    }

    t.Second = sec1;
    t.Minute = min1;
    t.Hour   = hr1;
    t.Day    = day1;
    t.Month  = mon1;

    if (cen1 != 0)
        t.Year = (u16)cen1 * 100 + yr1;
    else
        t.Year = DefaultCentury + yr1;

    Trace(0, "RTC: %u-%u-%u %u:%u:%u",
        (ulong)t.Year, (ulong)t.Month, (ulong)t.Day,
        (ulong)t.Hour, (ulong)t.Minute, (ulong)t.Second);

    return true;
}

ulong Rtc::ToUnixEpoch(const RtcTime& t)
{
    /* Days per month (non-leap) */
    static const u16 daysBeforeMonth[13] = {
        0, 0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334
    };

    if (t.Year < UnixEpochYear || t.Month < 1 || t.Month > MonthsPerYear || t.Day < 1)
        return 0;

    ulong y = t.Year;
    ulong m = t.Month;
    ulong d = t.Day;

    /* Days from epoch year to start of year y */
    ulong years = y - UnixEpochYear;
    ulong leapDays = 0;
    for (ulong i = UnixEpochYear; i < y; i++)
    {
        if ((i % 4 == 0 && i % 100 != 0) || (i % 400 == 0))
            leapDays++;
    }
    ulong days = years * 365 + leapDays;

    /* Days within the year */
    days += daysBeforeMonth[m];
    if (m > 2 && ((y % 4 == 0 && y % 100 != 0) || (y % 400 == 0)))
        days++;
    days += d - 1;

    return days * SecsPerDay + (ulong)t.Hour * SecsPerHour + (ulong)t.Minute * SecsPerMin + (ulong)t.Second;
}

}
