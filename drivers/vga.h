#pragma once

#include <include/types.h>
#include <kernel/spin_lock.h>
#include <lib/printer.h>

namespace Kernel
{

class VgaTerm : public Stdlib::Printer
{
public:
    static VgaTerm& GetInstance()
    {
        static VgaTerm Instance;
        return Instance;
    }

    void Puts(const char *s);
    void Cls();

    virtual void VPrintf(const char *fmt, va_list args) override;
    virtual void Printf(const char *fmt, ...) override;
    virtual void PrintString(const char *s) override;
    virtual void Backspace() override;

    /* VGA hardware text mode color constants. */
    enum Color {
        ColorBlack = 0,
        ColorBlue = 1,
        ColorGreen = 2,
        ColorCyan = 3,
        ColorRed = 4,
        ColorMagent = 5,
        ColorBrown = 6,
        ColorLightGray = 7,
        ColorDarkGray = 8,
        ColorLightBlud = 9,
        ColorLightGreen = 10,
        ColorLightCyan = 11,
        ColorLightRed = 12,
        ColorLightMagenta = 13,
        ColorLightBrown = 14,
        ColorWhite = 15,
    };

    void SetColor(Color fg, Color bg);

private:
    VgaTerm();
    virtual ~VgaTerm();

    VgaTerm(const VgaTerm& other) = delete;
    VgaTerm(VgaTerm&& other) = delete;
    VgaTerm& operator=(const VgaTerm& other) = delete;
    VgaTerm& operator=(VgaTerm&& other) = delete;

    u8 MakeColor(Color fg, Color bg);
    u16 MakeEntry(char c, u8 color);

    void PutCharAt(char c, u8 color, u8 x, u8 y);

    size_t GetIndex(u8 x, u8 y);

    void Overflow();

    void PutChar(char c);
    void Cursor();

    void PutsLockHeld(const char *s);
    void ClsLockHeld();

    const ulong BufPhyAddr = 0xB8000;

    static const u8 MaxWidth = 80;
    static const u8 MaxHeight = 25;

    u16 *Buf;
    u8 Row;
    u8 Column;
    u8 Width;
    u8 Height;
    u8 ColorCode;

    const u16 VgaBase = 0x3D4;
    const u16 VgaIndex = 0x0E;

    SpinLock Lock;
};

}
