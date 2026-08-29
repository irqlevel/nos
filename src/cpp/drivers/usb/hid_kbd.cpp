#include "hid_kbd.h"

#include <kernel/input.h>
#include <kernel/trace.h>
#include <lib/stdlib.h>

namespace Kernel
{
namespace Usb
{

/* HID Keyboard/Keypad usage page (0x07) -> character + the PS/2 set-1 make
   code the rest of the kernel already speaks. Consumers such as the shell
   key off the scan code (0x0E == backspace), so keeping the legacy codes
   makes a USB keyboard indistinguishable from the 8042 one. A zero
   character means "no printable character" and is never delivered. */
struct KeyMapEntry
{
    char Normal;
    char Shifted;
    u8 Ps2;
};

static const KeyMapEntry KeyMap[] =
{
    /* 0x00 */ { 0, 0, 0x00 },      /* Reserved / no event      */
    /* 0x01 */ { 0, 0, 0x00 },      /* ErrorRollOver            */
    /* 0x02 */ { 0, 0, 0x00 },      /* POSTFail                 */
    /* 0x03 */ { 0, 0, 0x00 },      /* ErrorUndefined           */
    /* 0x04 */ { 'a', 'A', 0x1E },
    /* 0x05 */ { 'b', 'B', 0x30 },
    /* 0x06 */ { 'c', 'C', 0x2E },
    /* 0x07 */ { 'd', 'D', 0x20 },
    /* 0x08 */ { 'e', 'E', 0x12 },
    /* 0x09 */ { 'f', 'F', 0x21 },
    /* 0x0A */ { 'g', 'G', 0x22 },
    /* 0x0B */ { 'h', 'H', 0x23 },
    /* 0x0C */ { 'i', 'I', 0x17 },
    /* 0x0D */ { 'j', 'J', 0x24 },
    /* 0x0E */ { 'k', 'K', 0x25 },
    /* 0x0F */ { 'l', 'L', 0x26 },
    /* 0x10 */ { 'm', 'M', 0x32 },
    /* 0x11 */ { 'n', 'N', 0x31 },
    /* 0x12 */ { 'o', 'O', 0x18 },
    /* 0x13 */ { 'p', 'P', 0x19 },
    /* 0x14 */ { 'q', 'Q', 0x10 },
    /* 0x15 */ { 'r', 'R', 0x13 },
    /* 0x16 */ { 's', 'S', 0x1F },
    /* 0x17 */ { 't', 'T', 0x14 },
    /* 0x18 */ { 'u', 'U', 0x16 },
    /* 0x19 */ { 'v', 'V', 0x2F },
    /* 0x1A */ { 'w', 'W', 0x11 },
    /* 0x1B */ { 'x', 'X', 0x2D },
    /* 0x1C */ { 'y', 'Y', 0x15 },
    /* 0x1D */ { 'z', 'Z', 0x2C },
    /* 0x1E */ { '1', '!', 0x02 },
    /* 0x1F */ { '2', '@', 0x03 },
    /* 0x20 */ { '3', '#', 0x04 },
    /* 0x21 */ { '4', '$', 0x05 },
    /* 0x22 */ { '5', '%', 0x06 },
    /* 0x23 */ { '6', '^', 0x07 },
    /* 0x24 */ { '7', '&', 0x08 },
    /* 0x25 */ { '8', '*', 0x09 },
    /* 0x26 */ { '9', '(', 0x0A },
    /* 0x27 */ { '0', ')', 0x0B },
    /* 0x28 */ { '\n', '\n', 0x1C },   /* Enter        */
    /* 0x29 */ { 0, 0, 0x01 },         /* Escape       */
    /* 0x2A */ { '\b', '\b', 0x0E },   /* Backspace    */
    /* 0x2B */ { '\t', '\t', 0x0F },   /* Tab          */
    /* 0x2C */ { ' ', ' ', 0x39 },     /* Space        */
    /* 0x2D */ { '-', '_', 0x0C },
    /* 0x2E */ { '=', '+', 0x0D },
    /* 0x2F */ { '[', '{', 0x1A },
    /* 0x30 */ { ']', '}', 0x1B },
    /* 0x31 */ { '\\', '|', 0x2B },
    /* 0x32 */ { '\\', '|', 0x2B },    /* non-US # / ~ */
    /* 0x33 */ { ';', ':', 0x27 },
    /* 0x34 */ { '\'', '"', 0x28 },
    /* 0x35 */ { '`', '~', 0x29 },
    /* 0x36 */ { ',', '<', 0x33 },
    /* 0x37 */ { '.', '>', 0x34 },
    /* 0x38 */ { '/', '?', 0x35 },
    /* 0x39 */ { 0, 0, 0x3A },         /* CapsLock     */
    /* 0x3A */ { 0, 0, 0x3B },         /* F1           */
    /* 0x3B */ { 0, 0, 0x3C },
    /* 0x3C */ { 0, 0, 0x3D },
    /* 0x3D */ { 0, 0, 0x3E },
    /* 0x3E */ { 0, 0, 0x3F },
    /* 0x3F */ { 0, 0, 0x40 },
    /* 0x40 */ { 0, 0, 0x41 },
    /* 0x41 */ { 0, 0, 0x42 },
    /* 0x42 */ { 0, 0, 0x43 },
    /* 0x43 */ { 0, 0, 0x44 },         /* F10          */
    /* 0x44 */ { 0, 0, 0x57 },         /* F11          */
    /* 0x45 */ { 0, 0, 0x58 },         /* F12          */
    /* 0x46 */ { 0, 0, 0x00 },         /* PrintScreen  */
    /* 0x47 */ { 0, 0, 0x46 },         /* ScrollLock   */
    /* 0x48 */ { 0, 0, 0x00 },         /* Pause        */
    /* 0x49 */ { 0, 0, 0x00 },         /* Insert       */
    /* 0x4A */ { 0, 0, 0x00 },         /* Home         */
    /* 0x4B */ { 0, 0, 0x00 },         /* PageUp       */
    /* 0x4C */ { 0, 0, 0x00 },         /* Delete       */
    /* 0x4D */ { 0, 0, 0x00 },         /* End          */
    /* 0x4E */ { 0, 0, 0x00 },         /* PageDown     */
    /* 0x4F */ { 0, 0, 0x00 },         /* Right arrow  */
    /* 0x50 */ { 0, 0, 0x00 },         /* Left arrow   */
    /* 0x51 */ { 0, 0, 0x00 },         /* Down arrow   */
    /* 0x52 */ { 0, 0, 0x00 },         /* Up arrow     */
    /* 0x53 */ { 0, 0, 0x45 },         /* NumLock      */
    /* 0x54 */ { '/', '/', 0x35 },     /* Keypad /     */
    /* 0x55 */ { '*', '*', 0x37 },
    /* 0x56 */ { '-', '-', 0x4A },
    /* 0x57 */ { '+', '+', 0x4E },
    /* 0x58 */ { '\n', '\n', 0x1C },   /* Keypad Enter */
    /* 0x59 */ { '1', '1', 0x4F },
    /* 0x5A */ { '2', '2', 0x50 },
    /* 0x5B */ { '3', '3', 0x51 },
    /* 0x5C */ { '4', '4', 0x4B },
    /* 0x5D */ { '5', '5', 0x4C },
    /* 0x5E */ { '6', '6', 0x4D },
    /* 0x5F */ { '7', '7', 0x47 },
    /* 0x60 */ { '8', '8', 0x48 },
    /* 0x61 */ { '9', '9', 0x49 },
    /* 0x62 */ { '0', '0', 0x52 },
    /* 0x63 */ { '.', '.', 0x53 },
    /* 0x64 */ { '\\', '|', 0x56 },    /* non-US \ / | */
};

static const u8 ModLeftShift = 1 << 1;
static const u8 ModRightShift = 1 << 5;

static const u8 UsageErrorRollOver = 0x01;
static const u8 UsageCapsLock = 0x39;

HidBootKeyboard::HidBootKeyboard()
{
    Reset();
}

void HidBootKeyboard::Reset()
{
    Stdlib::MemSet(Prev, 0, sizeof(Prev));
    CapsLock = false;
    RepeatUsage = 0;
    RepeatModifiers = 0;
    RepeatNextNs = 0;
}

bool HidBootKeyboard::InReport(const u8* report, u8 usage)
{
    for (u32 i = 2; i < ReportSize; i++)
    {
        if (report[i] == usage)
            return true;
    }
    return false;
}

bool HidBootKeyboard::Emit(u8 usage, u8 modifiers)
{
    if (usage >= Stdlib::ArraySize(KeyMap))
        return false;

    const KeyMapEntry& e = KeyMap[usage];

    if (usage == UsageCapsLock)
    {
        CapsLock = !CapsLock;
        return false;
    }

    bool shift = (modifiers & (ModLeftShift | ModRightShift)) != 0;

    char c = shift ? e.Shifted : e.Normal;

    /* Caps Lock affects letters only, and inverts the effect of Shift */
    if (CapsLock && e.Normal >= 'a' && e.Normal <= 'z')
        c = shift ? e.Normal : e.Shifted;

    if (c == '\0')
        return false;

    Trace(KbdLL, "UsbKbd: usage 0x%p char %c", (ulong)usage, c);

    KeyboardInput::GetInstance().Emit(c, e.Ps2);
    return true;
}

void HidBootKeyboard::OnReport(const u8* report, u32 len)
{
    if (len < ReportSize)
        return;

    /* The keyboard reports a rollover condition by filling every slot with
       ErrorRollOver; there is no key information in such a report. */
    if (report[2] == UsageErrorRollOver)
        return;

    u8 modifiers = report[0];
    u8 lastPressed = 0;

    for (u32 i = 2; i < ReportSize; i++)
    {
        u8 usage = report[i];
        if (usage == 0)
            continue;

        if (!InReport(Prev, usage))
        {
            /* Only character-producing keys arm auto-repeat: repeating a
               lock key would toggle it dozens of times a second. */
            if (Emit(usage, modifiers))
                lastPressed = usage;
        }
    }

    Stdlib::MemCpy(Prev, report, ReportSize);

    if (lastPressed != 0)
    {
        /* Newest press wins the repeat, matching every other keyboard */
        RepeatUsage = lastPressed;
        RepeatModifiers = modifiers;
        RepeatNextNs = 0;
    }
    else if (RepeatUsage != 0 && !InReport(Prev, RepeatUsage))
    {
        RepeatUsage = 0;
    }
}

void HidBootKeyboard::OnTick(ulong nowNs)
{
    if (RepeatUsage == 0)
        return;

    if (RepeatNextNs == 0)
    {
        RepeatNextNs = nowNs + RepeatDelayNs;
        return;
    }

    if (nowNs < RepeatNextNs)
        return;

    Emit(RepeatUsage, RepeatModifiers);
    RepeatNextNs = nowNs + RepeatPeriodNs;
}

}
}
