#pragma once

#include <include/types.h>

namespace Kernel
{
namespace Usb
{

/* Decoder for the 8-byte HID boot-protocol keyboard report:

       byte 0   modifier bitmap (L/R Ctrl, Shift, Alt, GUI)
       byte 1   reserved
       byte 2-7 up to six concurrently pressed usage codes

   Reports are level, not edge: a key is "down" for as long as its usage
   stays in the array. Presses are therefore found by diffing against the
   previous report, and auto-repeat is synthesised here because a boot
   keyboard never repeats on its own. */
class HidBootKeyboard
{
public:
    HidBootKeyboard();

    void Reset();

    /* Feed one report. Emits decoded characters into KeyboardInput. */
    void OnReport(const u8* report, u32 len);

    /* Drive auto-repeat; call on every poll tick with the boot-time
       nanosecond clock. */
    void OnTick(ulong nowNs);

    static const u32 ReportSize = 8;

private:
    HidBootKeyboard(const HidBootKeyboard& other) = delete;
    HidBootKeyboard& operator=(const HidBootKeyboard& other) = delete;

    /* Returns true when the usage produced a character (and is therefore
       eligible for auto-repeat). */
    bool Emit(u8 usage, u8 modifiers);
    static bool InReport(const u8* report, u8 usage);

    /* Auto-repeat: hold before the first repeat, then the repeat period. */
    static const ulong RepeatDelayNs = 400UL * 1000 * 1000;
    static const ulong RepeatPeriodNs = 40UL * 1000 * 1000;

    u8 Prev[ReportSize];
    bool CapsLock;

    u8 RepeatUsage;      /* 0 when nothing is held */
    u8 RepeatModifiers;
    ulong RepeatNextNs;
};

}
}
