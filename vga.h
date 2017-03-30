#pragma once

#include "types.h"

namespace Kernel
{

namespace Core
{

class VgaTerm
{
public:
    static VgaTerm& GetInstance()
    {
        static VgaTerm instance;
        return instance;
    }

    void Puts(const char *s);
    void Cls();
    void Vprintf(const char *fmt, va_list args);
    void Printf(const char *fmt, ...);

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

    void Overflow();

    void PutChar(char c);

    const ulong BufAddr = 0xB8000;

    u16 *Buf;
    u8 Row;
    u8 Column;
    u8 Width;
    u8 Height;
    u8 ColorCode;
};

}
}