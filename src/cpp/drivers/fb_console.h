#pragma once

#include <include/types.h>
#include <kernel/spin_lock.h>
#include <lib/printer.h>
#include <drivers/font8x16.h>

namespace Kernel
{

/* Linear pixel framebuffer handed over by the bootloader (UEFI GOP or
   VBE). Arch-neutral on purpose: the arch glue (drivers/screen.cpp on
   x86) fills this in from whatever the boot protocol reported. */
struct FbInfo
{
    ulong PhyAddr;
    u32 Pitch;      /* bytes per scanline */
    u32 Width;      /* pixels */
    u32 Height;     /* pixels */
    u8 Bpp;         /* bits per pixel: 16, 24 or 32 */
    u8 RedPos;
    u8 RedSize;
    u8 GreenPos;
    u8 GreenSize;
    u8 BluePos;
    u8 BlueSize;
};

/* Text console drawn into a pixel framebuffer with the 8x16 font. This is
   the only usable screen on UEFI, where there is no EGA text mode.

   Framebuffer memory is uncached (MapMmioRegion), so reads from it are
   expensive: the console never reads back. A character grid in RAM
   mirrors the screen (Cells) plus what is currently painted (Shown), so
   scrolling repaints only the cells that actually changed. */
class FbTerm : public Stdlib::Printer
{
public:
    static FbTerm& GetInstance()
    {
        static FbTerm Instance;
        return Instance;
    }

    /* Maps the framebuffer and clears the screen. Must run on the BSP
       before the APs are started (MapMmioRegion constraint) and after
       PageTable::SetupFreePagesList(). */
    bool Setup(const FbInfo& info);

    static bool IsReady() { return ReadyFlag; }

    void Cls();

    virtual void VPrintf(const char *fmt, va_list args) override;
    virtual void Printf(const char *fmt, ...) override;
    virtual void PrintString(const char *s) override;
    virtual void Backspace() override;

    void PanicPrintString(const char *s);

private:
    FbTerm();
    virtual ~FbTerm();

    FbTerm(const FbTerm& other) = delete;
    FbTerm(FbTerm&& other) = delete;
    FbTerm& operator=(const FbTerm& other) = delete;
    FbTerm& operator=(FbTerm&& other) = delete;

    u32 PackColor(u8 r, u8 g, u8 b);
    void PutPixel(u32 x, u32 y, u32 color);
    void MakeSpan(u64 *span, u8 bits, u32 fgColor, u32 bgColor);
    void BlitSpan(u32 x, u32 y, const u64 *span);
    void FillRect(u32 x, u32 y, u32 w, u32 h, u32 color);

    void DrawGlyph(u32 row, u32 col, char c);
    void DrawCursor(bool on);

    void PutsLockHeld(const char *s);
    void PutChar(char c);
    void NewLine();
    void Scroll();
    void ClsLockHeld();
    void Repaint();

    /* Cap the character grid so it fits a fixed allocation: the console is
       set up before the heap exists. A larger mode just leaves the right
       and bottom edges of the screen unused. */
    static const u32 MaxCols = 200;
    static const u32 MaxRows = 64;

    u8 *Fb;
    u32 Pitch;
    u32 Width;
    u32 Height;
    u32 BytesPerPixel;
    /* Scanlines are 8-byte aligned, so glyph spans can go out as 64-bit
       stores (see BlitSpan). */
    bool FastBlit;

    u32 FgColor;
    u32 BgColor;

    u32 Cols;
    u32 Rows;
    u32 Row;
    u32 Column;

    FbInfo Info;

    char Cells[MaxRows][MaxCols];
    char Shown[MaxRows][MaxCols];

    SpinLock Lock;

    static bool ReadyFlag;
};

}
