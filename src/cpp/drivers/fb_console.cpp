#include "fb_console.h"

#include <kernel/watchdog.h>
#include <kernel/trace.h>
#include <lib/stdlib.h>
#include <mm/page_table.h>
#include <drivers/mmio.h>
#include <hal/barrier.h>

namespace Kernel
{

bool FbTerm::ReadyFlag = false;

FbTerm::FbTerm()
    : Fb(nullptr)
    , Pitch(0)
    , Width(0)
    , Height(0)
    , BytesPerPixel(0)
    , FastBlit(false)
    , FgColor(0)
    , BgColor(0)
    , Cols(0)
    , Rows(0)
    , Row(0)
    , Column(0)
{
    Stdlib::MemSet(&Info, 0, sizeof(Info));
}

FbTerm::~FbTerm()
{
    /* The framebuffer mapping is permanent (MapMmioRegion), so there is
       nothing to release; the console lives for the whole boot. */
}

u32 FbTerm::PackColor(u8 r, u8 g, u8 b)
{
    u32 value = 0;

    if (Info.RedSize != 0 && Info.RedSize <= 8)
        value |= (u32)(r >> (8 - Info.RedSize)) << Info.RedPos;
    if (Info.GreenSize != 0 && Info.GreenSize <= 8)
        value |= (u32)(g >> (8 - Info.GreenSize)) << Info.GreenPos;
    if (Info.BlueSize != 0 && Info.BlueSize <= 8)
        value |= (u32)(b >> (8 - Info.BlueSize)) << Info.BluePos;

    return value;
}

void FbTerm::PutPixel(u32 x, u32 y, u32 color)
{
    u8 *p = Fb + (ulong)y * Pitch + (ulong)x * BytesPerPixel;

    switch (BytesPerPixel)
    {
    case 4:
        MmioWrite32(p, color);
        break;
    case 3:
        MmioWrite8(p, (u8)color);
        MmioWrite8(p + 1, (u8)(color >> 8));
        MmioWrite8(p + 2, (u8)(color >> 16));
        break;
    case 2:
        MmioWrite16(p, (u16)color);
        break;
    default:
        break;
    }
}

/* Framebuffer memory is uncached, so drawing cost is bus transactions, not
   bytes: a 24bpp pixel written as three MmioWrite8 costs three of them.
   One font cell is 8 pixels wide, i.e. 16, 24 or 32 bytes -- always a
   multiple of 8 -- so a whole glyph row is staged in a register-aligned
   buffer and pushed out as 64-bit stores instead. */
void FbTerm::MakeSpan(u64 *span, u8 bits, u32 fgColor, u32 bgColor)
{
    u8 *p = (u8 *)span;

    for (u32 i = 0; i < Font8x16Width; i++)
    {
        u32 color = (bits & (0x80 >> i)) ? fgColor : bgColor;

        switch (BytesPerPixel)
        {
        case 4:
            *(u32 *)(p + i * 4) = color;
            break;
        case 3:
            p[i * 3] = (u8)color;
            p[i * 3 + 1] = (u8)(color >> 8);
            p[i * 3 + 2] = (u8)(color >> 16);
            break;
        case 2:
            *(u16 *)(p + i * 2) = (u16)color;
            break;
        default:
            break;
        }
    }
}

void FbTerm::BlitSpan(u32 x, u32 y, const u64 *span)
{
    u32 bytes = Font8x16Width * BytesPerPixel;
    u8 *dst = Fb + (ulong)y * Pitch + (ulong)x * BytesPerPixel;

    if (!FastBlit)
    {
        const u8 *src = (const u8 *)span;
        for (u32 i = 0; i < bytes; i++)
            MmioWrite8(dst + i, src[i]);
        return;
    }

    for (u32 i = 0; i < bytes / sizeof(u64); i++)
        MmioWrite64(dst + i * sizeof(u64), span[i]);
}

void FbTerm::FillRect(u32 x, u32 y, u32 w, u32 h, u32 color)
{
    u64 span[Font8x16Width * 4 / sizeof(u64)];
    u32 spans = 0;

    /* Wide 8-pixel stores need the span to start on a cell boundary */
    if ((x % Font8x16Width) == 0)
    {
        MakeSpan(span, 0, color, color);
        spans = w / Font8x16Width;
    }

    for (u32 dy = 0; dy < h; dy++)
    {
        for (u32 i = 0; i < spans; i++)
            BlitSpan(x + i * Font8x16Width, y + dy, span);

        for (u32 dx = spans * Font8x16Width; dx < w; dx++)
            PutPixel(x + dx, y + dy, color);
    }
}

void FbTerm::DrawGlyph(u32 row, u32 col, char c)
{
    u64 span[Font8x16Width * 4 / sizeof(u64)];
    u32 x0 = col * Font8x16Width;
    u32 y0 = row * Font8x16Height;

    if (c < Font8x16FirstChar || c > Font8x16LastChar)
    {
        /* Unprintable, including the '\0' the grid is cleared with */
        FillRect(x0, y0, Font8x16Width, Font8x16Height, BgColor);
        return;
    }

    const u8 *glyph = Font8x16[(u32)(c - Font8x16FirstChar)];

    for (u32 dy = 0; dy < Font8x16Height; dy++)
    {
        MakeSpan(span, glyph[dy], FgColor, BgColor);
        BlitSpan(x0, y0 + dy, span);
    }
}

void FbTerm::DrawCursor(bool on)
{
    /* Underline cursor: the bottom two pixel rows of the current cell */
    const u32 CursorHeight = 2;

    if (Row >= Rows || Column >= Cols)
        return;

    if (!on)
    {
        /* Repaint the cell instead of clearing those rows: the underline
           sits exactly where the font draws '_' and the descenders, so
           painting background over it would eat part of the glyph. */
        DrawGlyph(Row, Column, Cells[Row][Column]);
        return;
    }

    FillRect(Column * Font8x16Width,
        Row * Font8x16Height + Font8x16Height - CursorHeight,
        Font8x16Width, CursorHeight, FgColor);
}

void FbTerm::Repaint()
{
    for (u32 row = 0; row < Rows; row++)
    {
        for (u32 col = 0; col < Cols; col++)
        {
            if (Cells[row][col] == Shown[row][col])
                continue;

            DrawGlyph(row, col, Cells[row][col]);
            Shown[row][col] = Cells[row][col];
        }
    }
}

void FbTerm::Scroll()
{
    for (u32 row = 0; row + 1 < Rows; row++)
        Stdlib::MemCpy(Cells[row], Cells[row + 1], Cols);

    Stdlib::MemSet(Cells[Rows - 1], 0, Cols);

    Repaint();
}

void FbTerm::NewLine()
{
    Column = 0;
    Row++;
    if (Row == Rows)
    {
        Scroll();
        Row = Rows - 1;
    }
}

void FbTerm::PutChar(char c)
{
    if (c == '\n')
    {
        NewLine();
        return;
    }

    if (c == '\r')
    {
        Column = 0;
        return;
    }

    Cells[Row][Column] = c;
    if (Shown[Row][Column] != c)
    {
        DrawGlyph(Row, Column, c);
        Shown[Row][Column] = c;
    }

    Column++;
    if (Column == Cols)
        NewLine();
}

void FbTerm::PutsLockHeld(const char *str)
{
    DrawCursor(false);

    for (;;)
    {
        char c = *str++;
        if (c == '\0')
            break;

        PutChar(c);
    }

    DrawCursor(true);

    /* Write-combining stores may sit in the CPU's fill buffers; without
       this the last line of output can stay invisible indefinitely. */
    Hal::WcFlush();
}

void FbTerm::ClsLockHeld()
{
    Stdlib::MemSet(Cells, 0, sizeof(Cells));
    Stdlib::MemSet(Shown, 0, sizeof(Shown));

    FillRect(0, 0, Width, Height, BgColor);

    Row = 0;
    Column = 0;
}

bool FbTerm::Setup(const FbInfo& info)
{
    Watchdog::GetInstance().UnregisterSpinLock(Lock);

    Stdlib::AutoLock lock(Lock);

    if (ReadyFlag)
        return true;

    if (info.Bpp != 32 && info.Bpp != 24 && info.Bpp != 16)
    {
        Trace(0, "Fb: unsupported bpp %u", (ulong)info.Bpp);
        return false;
    }

    if (info.Width < Font8x16Width || info.Height < Font8x16Height)
    {
        Trace(0, "Fb: mode too small %ux%u", (ulong)info.Width, (ulong)info.Height);
        return false;
    }

    if (info.PhyAddr == 0 || (info.PhyAddr & (Const::PageSize - 1)) != 0)
    {
        Trace(0, "Fb: unaligned framebuffer 0x%p", info.PhyAddr);
        return false;
    }

    if (info.Pitch < (ulong)info.Width * (info.Bpp / 8))
    {
        Trace(0, "Fb: bad pitch %u for bpp %u", (ulong)info.Pitch, (ulong)info.Bpp);
        return false;
    }

    /* Framebuffer memory has no read side effects, so it is mapped
       write-combining: stores get buffered and merged instead of going out
       one bus transaction at a time. Everything that draws ends with
       Hal::WcFlush() so the pixels actually appear. */
    ulong sizeBytes = (ulong)info.Pitch * info.Height;
    ulong va = Mm::PageTable::GetInstance().MapMmioRegion(info.PhyAddr, sizeBytes,
        Mm::PageTable::MmioWriteCombining);
    if (va == 0)
    {
        Trace(0, "Fb: can't map framebuffer 0x%p size %u", info.PhyAddr, sizeBytes);
        return false;
    }

    Info = info;
    Fb = (u8 *)va;
    Pitch = info.Pitch;
    Width = info.Width;
    Height = info.Height;
    BytesPerPixel = info.Bpp / 8;
    FastBlit = ((Pitch % sizeof(u64)) == 0);

    Cols = Width / Font8x16Width;
    Rows = Height / Font8x16Height;
    if (Cols > MaxCols)
        Cols = MaxCols;
    if (Rows > MaxRows)
        Rows = MaxRows;

    FgColor = PackColor(0xAA, 0xAA, 0xAA);
    BgColor = PackColor(0x00, 0x00, 0x00);

    ClsLockHeld();
    DrawCursor(true);
    Hal::WcFlush();

    ReadyFlag = true;

    Trace(0, "Fb: %ux%u bpp %u pitch %u phys 0x%p va 0x%p grid %ux%u",
        (ulong)Width, (ulong)Height, (ulong)info.Bpp, (ulong)Pitch,
        info.PhyAddr, va, (ulong)Cols, (ulong)Rows);

    return true;
}

void FbTerm::Cls()
{
    Stdlib::AutoLock lock(Lock);

    if (Fb == nullptr)
        return;

    ClsLockHeld();
    DrawCursor(true);
    Hal::WcFlush();
}

void FbTerm::VPrintf(const char *fmt, va_list args)
{
    char str[256];

    if (Stdlib::VsnPrintf(str, sizeof(str), fmt, args) < 0)
        return;

    Stdlib::AutoLock lock(Lock);

    if (Fb == nullptr)
        return;

    PutsLockHeld(str);
}

void FbTerm::Printf(const char *fmt, ...)
{
    va_list args;

    va_start(args, fmt);
    VPrintf(fmt, args);
    va_end(args);
}

void FbTerm::PrintString(const char *s)
{
    Stdlib::AutoLock lock(Lock);

    if (Fb == nullptr)
        return;

    PutsLockHeld(s);
}

void FbTerm::PanicPrintString(const char *s)
{
    /*
     * Bypass the lock -- draw straight into the framebuffer.
     * Safe only in panic context with interrupts disabled.
     */
    if (Fb == nullptr)
        return;

    PutsLockHeld(s);
}

void FbTerm::Backspace()
{
    Stdlib::AutoLock lock(Lock);

    if (Fb == nullptr)
        return;

    DrawCursor(false);

    if (Column > 0)
    {
        Column--;
    }
    else if (Row > 0)
    {
        Row--;
        Column = Cols - 1;
    }

    Cells[Row][Column] = '\0';
    if (Shown[Row][Column] != '\0')
    {
        DrawGlyph(Row, Column, '\0');
        Shown[Row][Column] = '\0';
    }

    DrawCursor(true);
    Hal::WcFlush();
}

}
