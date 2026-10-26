#include "rtc.h"
#include "acpi.h"

#include <arch/x86_64/asm.h>
#include <kernel/trace.h>

namespace kernel
{

Rtc::Rtc()
{
}

Rtc::~Rtc()
{
}

u8 Rtc::read_register(u8 reg)
{
    outb(INDEX_PORT, reg);
    return inb(DATA_PORT);
}

u8 Rtc::bcd_to_bin(u8 bcd)
{
    return (bcd & 0x0F) + ((bcd >> 4) * 10);
}

bool Rtc::read_time(RtcTime& t)
{
    /* Wait until update-in-progress clears */
    ulong timeout = UIP_TIMEOUT;
    while ((read_register(REG_STATUS_A) & STATUS_A_UIP) && timeout > 0)
        timeout--;

    /* Read first snapshot */
    u8 sec1 = read_register(REG_SECONDS);
    u8 min1 = read_register(REG_MINUTES);
    u8 hr1  = read_register(REG_HOURS);
    u8 day1 = read_register(REG_DAY);
    u8 mon1 = read_register(REG_MONTH);
    u8 yr1  = read_register(REG_YEAR);
    u8 cen1 = read_register(REG_CENTURY);

    /* Read second snapshot and compare to avoid mid-update inconsistency */
    u8 sec2, min2, hr2, day2, mon2, yr2, cen2;
    do {
        sec2 = sec1; min2 = min1; hr2 = hr1;
        day2 = day1; mon2 = mon1; yr2 = yr1; cen2 = cen1;

        /* Wait for update-in-progress to clear again */
        timeout = UIP_TIMEOUT;
        while ((read_register(REG_STATUS_A) & STATUS_A_UIP) && timeout > 0)
            timeout--;

        sec1 = read_register(REG_SECONDS);
        min1 = read_register(REG_MINUTES);
        hr1  = read_register(REG_HOURS);
        day1 = read_register(REG_DAY);
        mon1 = read_register(REG_MONTH);
        yr1  = read_register(REG_YEAR);
        cen1 = read_register(REG_CENTURY);
    } while (sec1 != sec2 || min1 != min2 || hr1 != hr2 ||
             day1 != day2 || mon1 != mon2 || yr1 != yr2 || cen1 != cen2);

    u8 status_b = read_register(REG_STATUS_B);
    bool binary_mode = (status_b & STATUS_B_BINARY) != 0;
    bool mode24h    = (status_b & STATUS_B24_HOUR) != 0;

    if (!binary_mode)
    {
        sec1 = bcd_to_bin(sec1);
        min1 = bcd_to_bin(min1);
        hr1  = bcd_to_bin(hr1 & HOUR_MASK) | (hr1 & HOUR_PM_BIT); /* preserve PM bit */
        day1 = bcd_to_bin(day1);
        mon1 = bcd_to_bin(mon1);
        yr1  = bcd_to_bin(yr1);
        cen1 = bcd_to_bin(cen1);
    }

    if (!mode24h)
    {
        if (hr1 & HOUR_PM_BIT)
            hr1 = ((hr1 & HOUR_MASK) % HOURS_PER_HALF_DAY) + HOURS_PER_HALF_DAY;
        else if ((hr1 & HOUR_MASK) == HOURS_PER_HALF_DAY)
            hr1 = 0; /* 12 AM = midnight */
    }

    t.second = sec1;
    t.minute = min1;
    t.hour   = hr1;
    t.day    = day1;
    t.month  = mon1;

    /* Only trust the century CMOS register if the FADT advertises one;
       on a board without a century register, REG_CENTURY (0x32) holds
       unrelated data and would yield a wildly wrong year. */
    u8 century_reg = Acpi::get_instance().get_century_register();
    if (century_reg != 0 && cen1 != 0)
        t.year = (u16)cen1 * 100 + yr1;
    else
        t.year = DEFAULT_CENTURY + yr1;

    trace(0, "RTC: %lu-%lu-%lu %lu:%lu:%lu",
        (ulong)t.year, (ulong)t.month, (ulong)t.day,
        (ulong)t.hour, (ulong)t.minute, (ulong)t.second);

    return true;
}

ulong Rtc::to_unix_epoch(const RtcTime& t)
{
    /* Days per month (non-leap) */
    static const u16 DAYS_BEFORE_MONTH[13] = {
        0, 0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334
    };

    if (t.year < UNIX_EPOCH_YEAR || t.month < 1 || t.month > MONTHS_PER_YEAR || t.day < 1)
        return 0;

    ulong y = t.year;
    ulong m = t.month;
    ulong d = t.day;

    /* Days from epoch year to start of year y */
    ulong years = y - UNIX_EPOCH_YEAR;
    ulong leap_days = 0;
    for (ulong i = UNIX_EPOCH_YEAR; i < y; i++)
    {
        if ((i % 4 == 0 && i % 100 != 0) || (i % 400 == 0))
            leap_days++;
    }
    ulong days = years * 365 + leap_days;

    /* Days within the year */
    days += DAYS_BEFORE_MONTH[m];
    if (m > 2 && ((y % 4 == 0 && y % 100 != 0) || (y % 400 == 0)))
        days++;
    days += d - 1;

    return days * SECS_PER_DAY + (ulong)t.hour * SECS_PER_HOUR + (ulong)t.minute * SECS_PER_MIN + (ulong)t.second;
}

}
