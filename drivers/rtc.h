#pragma once

#include <include/types.h>

namespace Kernel
{

struct RtcTime
{
    u16 Year;
    u8 Month;
    u8 Day;
    u8 Hour;
    u8 Minute;
    u8 Second;
};

class Rtc
{
public:
    static Rtc& GetInstance()
    {
        static Rtc instance;
        return instance;
    }

    bool ReadTime(RtcTime& t);
    static ulong ToUnixEpoch(const RtcTime& t);

private:
    Rtc();
    ~Rtc();
    Rtc(const Rtc& other) = delete;
    Rtc(Rtc&& other) = delete;
    Rtc& operator=(const Rtc& other) = delete;
    Rtc& operator=(Rtc&& other) = delete;

    static const u16 IndexPort = 0x70;
    static const u16 DataPort  = 0x71;

    static const u8 RegSeconds  = 0x00;
    static const u8 RegMinutes  = 0x02;
    static const u8 RegHours    = 0x04;
    static const u8 RegDay      = 0x07;
    static const u8 RegMonth    = 0x08;
    static const u8 RegYear     = 0x09;
    static const u8 RegCentury  = 0x32;
    static const u8 RegStatusA  = 0x0A;
    static const u8 RegStatusB  = 0x0B;

    /* StatusA/B bit masks */
    static const u8 StatusAUip       = 0x80; /* Update-In-Progress */
    static const u8 StatusBBinary    = 0x04; /* binary mode (vs BCD) */
    static const u8 StatusB24Hour   = 0x02; /* 24-hour mode */

    /* 12-hour mode bits */
    static const u8 HourPmBit  = 0x80;
    static const u8 HourMask   = 0x7F;
    static const u8 HoursPerHalfDay = 12;

    static const ulong UipTimeout = 10000;
    static const u16 DefaultCentury = 2000;
    static const u16 UnixEpochYear = 1970;
    static const ulong SecsPerDay  = 86400;
    static const ulong SecsPerHour = 3600;
    static const ulong SecsPerMin  = 60;
    static const ulong MonthsPerYear = 12;

    u8 ReadRegister(u8 reg);
    static u8 BcdToBin(u8 bcd);
};

}
