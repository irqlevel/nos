#pragma once

#include <include/types.h>

namespace kernel
{

struct RtcTime
{
    u16 year;
    u8 month;
    u8 day;
    u8 hour;
    u8 minute;
    u8 second;
};

class Rtc
{
public:
    static Rtc& get_instance()
    {
        static Rtc instance;
        return instance;
    }

    bool read_time(RtcTime& t);
    static ulong to_unix_epoch(const RtcTime& t);

private:
    Rtc();
    ~Rtc();
    Rtc(const Rtc& other) = delete;
    Rtc(Rtc&& other) = delete;
    Rtc& operator=(const Rtc& other) = delete;
    Rtc& operator=(Rtc&& other) = delete;

    static const u16 INDEX_PORT = 0x70;
    static const u16 DATA_PORT  = 0x71;

    static const u8 REG_SECONDS  = 0x00;
    static const u8 REG_MINUTES  = 0x02;
    static const u8 REG_HOURS    = 0x04;
    static const u8 REG_DAY      = 0x07;
    static const u8 REG_MONTH    = 0x08;
    static const u8 REG_YEAR     = 0x09;
    static const u8 REG_CENTURY  = 0x32;
    static const u8 REG_STATUS_A  = 0x0A;
    static const u8 REG_STATUS_B  = 0x0B;

    /* StatusA/B bit masks */
    static const u8 STATUS_A_UIP       = 0x80; /* Update-In-Progress */
    static const u8 STATUS_B_BINARY    = 0x04; /* binary mode (vs BCD) */
    static const u8 STATUS_B24_HOUR   = 0x02; /* 24-hour mode */

    /* 12-hour mode bits */
    static const u8 HOUR_PM_BIT  = 0x80;
    static const u8 HOUR_MASK   = 0x7F;
    static const u8 HOURS_PER_HALF_DAY = 12;

    static const ulong UIP_TIMEOUT = 10000;
    static const u16 DEFAULT_CENTURY = 2000;
    static const u16 UNIX_EPOCH_YEAR = 1970;
    static const ulong SECS_PER_DAY  = 86400;
    static const ulong SECS_PER_HOUR = 3600;
    static const ulong SECS_PER_MIN  = 60;
    static const ulong MONTHS_PER_YEAR = 12;

    u8 read_register(u8 reg);
    static u8 bcd_to_bin(u8 bcd);
};

}
