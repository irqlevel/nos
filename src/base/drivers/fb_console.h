#pragma once

#include <include/types.h>
#include <kernel/atomic.h>
#include <kernel/spin_lock.h>
#include <lib/printer.h>
#include <drivers/font8x16.h>

namespace kernel
{

/* Linear pixel framebuffer handed over by the bootloader (UEFI GOP or
   VBE). Arch-neutral on purpose: the arch glue (drivers/screen.cpp on
   x86) fills this in from whatever the boot protocol reported. */
struct FbInfo
{
    ulong phy_addr;
    u32 pitch;      /* bytes per scanline */
    u32 width;      /* pixels */
    u32 height;     /* pixels */
    u8 bpp;         /* bits per pixel: 16, 24 or 32 */
    u8 red_pos;
    u8 red_size;
    u8 green_pos;
    u8 green_size;
    u8 blue_pos;
    u8 blue_size;
};

/* Text console drawn into a pixel framebuffer with the 8x16 font. This is
   the only usable screen on UEFI, where there is no EGA text mode.

   Framebuffer memory is uncached (map_mmio_region), so reads from it are
   expensive: the console never reads back. A character grid in RAM
   mirrors the screen (Cells) plus what is currently painted (Shown), so
   scrolling repaints only the cells that actually changed. */
class FbTerm : public stdlib::Printer
{
public:
    static FbTerm& get_instance()
    {
        static FbTerm instance;
        return instance;
    }

    /* Maps the framebuffer and clears the screen. Must run on the BSP
       before the APs are started (map_mmio_region constraint) and after
       PageTable::setup_free_pages_list(). */
    bool setup(const FbInfo& info);

    static bool is_ready() { return ready_flag_; }

    void cls();

    virtual PRINTF_FORMAT(2, 0) void v_printf(const char *fmt, va_list args) override;
    virtual PRINTF_FORMAT(2, 3) void printf(const char *fmt, ...) override;
    virtual void print_string(const char *s) override;
    virtual void backspace() override;

    void panic_print_string(const char *s);

    /* The framebuffer let go of for good, should any of it lie in
       [base, base + size): a device's BAR, the device about to be given to
       a guest (hv's pci=) -- the boot GPU, whose memory the firmware's
       framebuffer is. Nothing is drawn there from then on, the panic's
       report included; the mapping stays, and is not written. True when it
       held one there. */
    bool release_within(ulong base, ulong size);

    /* Drawing paused (hold) and taken up again (unhold), the two paired:
       around a stretch in which some device's memory does not answer -- a
       function about to be given to a guest, its decoding off while its BARs
       are sized, before release_within can say whether the framebuffer is
       its. What would have been drawn meanwhile is not; the log has it. */
    void hold();
    void unhold();

private:
    FbTerm();
    virtual ~FbTerm();

    FbTerm(const FbTerm& other) = delete;
    FbTerm(FbTerm&& other) = delete;
    FbTerm& operator=(const FbTerm& other) = delete;
    FbTerm& operator=(FbTerm&& other) = delete;

    u32 pack_color(u8 r, u8 g, u8 b);
    void put_pixel(u32 x, u32 y, u32 color);
    void make_span(u64 *span, u8 bits, u32 fg_color, u32 bg_color);
    void blit_span(u32 x, u32 y, const u64 *span);
    void fill_rect(u32 x, u32 y, u32 w, u32 h, u32 color);

    /* Where the next character goes. Kept in row_/column_ between calls and
       worked on as a copy within one: the locked path's is the members' as
       they are, and the panic's -- which takes no lock, while another CPU
       may hold it in the middle of a line feed, row_ one past the grid
       until its scroll is done -- is read once and held to the grid, so that
       no cell it names is outside the arrays or the screen. */
    struct Cursor
    {
        u32 row;
        u32 column;
    };

    void draw_glyph(u32 row, u32 col, char c);
    void draw_cursor(const Cursor& at, bool on);

    void puts_lock_held(const char *s);
    /* The text drawn from `at` on, the cursor after it, and where that left
       it kept: `at` inside the grid */
    void puts_from(Cursor at, const char *s);
    void put_char(Cursor& at, char c);
    void new_line(Cursor& at);
    void scroll();
    void cls_lock_held();
    void repaint();
    /* Whether there is a framebuffer to draw into: one set up, not
       released, not held */
    bool drawing();

    /* Cap the character grid so it fits a fixed allocation: the console is
       set up before the heap exists. A larger mode just leaves the right
       and bottom edges of the screen unused. */
    static const u32 MAX_COLS = 200;
    static const u32 MAX_ROWS = 64;

    u8 *fb_;
    u32 pitch_;
    u32 width_;
    u32 height_;
    u32 bytes_per_pixel_;
    /* Scanlines are 8-byte aligned, so glyph spans can go out as 64-bit
       stores (see blit_span). */
    bool fast_blit_;

    u32 fg_color_;
    u32 bg_color_;

    u32 cols_;
    u32 rows_;
    u32 row_;
    u32 column_;

    FbInfo info_;

    char cells_[MAX_ROWS][MAX_COLS];
    char shown_[MAX_ROWS][MAX_COLS];

    SpinLock lock_;
    /* Set once, by release_within; and the holds there are. Changed under
       lock_ and read under it by every path that draws -- and by the
       panic's, which takes no lock */
    Atomic released_;
    Atomic holds_;

    static bool ready_flag_;
};

}
