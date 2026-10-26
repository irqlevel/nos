#include "fb_console.h"

#include <kernel/watchdog.h>
#include <kernel/trace.h>
#include <lib/stdlib.h>
#include <mm/page_table.h>
#include <drivers/mmio.h>
#include <hal/barrier.h>

namespace kernel
{

bool FbTerm::ready_flag_ = false;

FbTerm::FbTerm()
    : fb_(nullptr)
    , pitch_(0)
    , width_(0)
    , height_(0)
    , bytes_per_pixel_(0)
    , fast_blit_(false)
    , fg_color_(0)
    , bg_color_(0)
    , cols_(0)
    , rows_(0)
    , row_(0)
    , column_(0)
{
    stdlib::mem_set(&info_, 0, sizeof(info_));
}

FbTerm::~FbTerm()
{
    /* The framebuffer mapping is permanent (map_mmio_region), so there is
       nothing to release; the console lives for the whole boot. */
}

u32 FbTerm::pack_color(u8 r, u8 g, u8 b)
{
    u32 value = 0;

    if (info_.red_size != 0 && info_.red_size <= 8)
        value |= (u32)(r >> (8 - info_.red_size)) << info_.red_pos;
    if (info_.green_size != 0 && info_.green_size <= 8)
        value |= (u32)(g >> (8 - info_.green_size)) << info_.green_pos;
    if (info_.blue_size != 0 && info_.blue_size <= 8)
        value |= (u32)(b >> (8 - info_.blue_size)) << info_.blue_pos;

    return value;
}

void FbTerm::put_pixel(u32 x, u32 y, u32 color)
{
    u8 *p = fb_ + (ulong)y * pitch_ + (ulong)x * bytes_per_pixel_;

    switch (bytes_per_pixel_)
    {
    case 4:
        mmio_write32(p, color);
        break;
    case 3:
        mmio_write8(p, (u8)color);
        mmio_write8(p + 1, (u8)(color >> 8));
        mmio_write8(p + 2, (u8)(color >> 16));
        break;
    case 2:
        mmio_write16(p, (u16)color);
        break;
    default:
        break;
    }
}

/* Framebuffer memory is uncached, so drawing cost is bus transactions, not
   bytes: a 24bpp pixel written as three mmio_write8 costs three of them.
   One font cell is 8 pixels wide, i.e. 16, 24 or 32 bytes -- always a
   multiple of 8 -- so a whole glyph row is staged in a register-aligned
   buffer and pushed out as 64-bit stores instead. */
void FbTerm::make_span(u64 *span, u8 bits, u32 fg_color, u32 bg_color)
{
    u8 *p = (u8 *)span;

    for (u32 i = 0; i < FONT8X16_WIDTH; i++)
    {
        u32 color = (bits & (0x80 >> i)) ? fg_color : bg_color;

        switch (bytes_per_pixel_)
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

void FbTerm::blit_span(u32 x, u32 y, const u64 *span)
{
    u32 bytes = FONT8X16_WIDTH * bytes_per_pixel_;
    u8 *dst = fb_ + (ulong)y * pitch_ + (ulong)x * bytes_per_pixel_;

    if (!fast_blit_)
    {
        const u8 *src = (const u8 *)span;
        for (u32 i = 0; i < bytes; i++)
            mmio_write8(dst + i, src[i]);
        return;
    }

    for (u32 i = 0; i < bytes / sizeof(u64); i++)
        mmio_write64(dst + i * sizeof(u64), span[i]);
}

void FbTerm::fill_rect(u32 x, u32 y, u32 w, u32 h, u32 color)
{
    u64 span[FONT8X16_WIDTH * 4 / sizeof(u64)];
    u32 spans = 0;

    /* Wide 8-pixel stores need the span to start on a cell boundary */
    if ((x % FONT8X16_WIDTH) == 0)
    {
        make_span(span, 0, color, color);
        spans = w / FONT8X16_WIDTH;
    }

    for (u32 dy = 0; dy < h; dy++)
    {
        for (u32 i = 0; i < spans; i++)
            blit_span(x + i * FONT8X16_WIDTH, y + dy, span);

        for (u32 dx = spans * FONT8X16_WIDTH; dx < w; dx++)
            put_pixel(x + dx, y + dy, color);
    }
}

void FbTerm::draw_glyph(u32 row, u32 col, char c)
{
    u64 span[FONT8X16_WIDTH * 4 / sizeof(u64)];
    u32 x0 = col * FONT8X16_WIDTH;
    u32 y0 = row * FONT8X16_HEIGHT;

    if (c < FONT8X16_FIRST_CHAR || c > FONT8X16_LAST_CHAR)
    {
        /* Unprintable, including the '\0' the grid is cleared with */
        fill_rect(x0, y0, FONT8X16_WIDTH, FONT8X16_HEIGHT, bg_color_);
        return;
    }

    const u8 *glyph = FONT8X16[(u32)(c - FONT8X16_FIRST_CHAR)];

    for (u32 dy = 0; dy < FONT8X16_HEIGHT; dy++)
    {
        make_span(span, glyph[dy], fg_color_, bg_color_);
        blit_span(x0, y0 + dy, span);
    }
}

void FbTerm::draw_cursor(bool on)
{
    /* Underline cursor: the bottom two pixel rows of the current cell */
    const u32 cursor_height = 2;

    if (row_ >= rows_ || column_ >= cols_)
        return;

    if (!on)
    {
        /* Repaint the cell instead of clearing those rows: the underline
           sits exactly where the font draws '_' and the descenders, so
           painting background over it would eat part of the glyph. */
        draw_glyph(row_, column_, cells_[row_][column_]);
        return;
    }

    fill_rect(column_ * FONT8X16_WIDTH,
        row_ * FONT8X16_HEIGHT + FONT8X16_HEIGHT - cursor_height,
        FONT8X16_WIDTH, cursor_height, fg_color_);
}

void FbTerm::repaint()
{
    for (u32 row = 0; row < rows_; row++)
    {
        for (u32 col = 0; col < cols_; col++)
        {
            if (cells_[row][col] == shown_[row][col])
                continue;

            draw_glyph(row, col, cells_[row][col]);
            shown_[row][col] = cells_[row][col];
        }
    }
}

void FbTerm::scroll()
{
    for (u32 row = 0; row + 1 < rows_; row++)
        stdlib::mem_cpy(cells_[row], cells_[row + 1], cols_);

    stdlib::mem_set(cells_[rows_ - 1], 0, cols_);

    repaint();
}

void FbTerm::new_line()
{
    column_ = 0;
    row_++;
    if (row_ == rows_)
    {
        scroll();
        row_ = rows_ - 1;
    }
}

void FbTerm::put_char(char c)
{
    if (c == '\n')
    {
        new_line();
        return;
    }

    if (c == '\r')
    {
        column_ = 0;
        return;
    }

    cells_[row_][column_] = c;
    if (shown_[row_][column_] != c)
    {
        draw_glyph(row_, column_, c);
        shown_[row_][column_] = c;
    }

    column_++;
    if (column_ == cols_)
        new_line();
}

void FbTerm::puts_lock_held(const char *str)
{
    draw_cursor(false);

    for (;;)
    {
        char c = *str++;
        if (c == '\0')
            break;

        put_char(c);
    }

    draw_cursor(true);

    /* Write-combining stores may sit in the CPU's fill buffers; without
       this the last line of output can stay invisible indefinitely. */
    hal::wc_flush();
}

void FbTerm::cls_lock_held()
{
    stdlib::mem_set(cells_, 0, sizeof(cells_));
    stdlib::mem_set(shown_, 0, sizeof(shown_));

    fill_rect(0, 0, width_, height_, bg_color_);

    row_ = 0;
    column_ = 0;
}

bool FbTerm::setup(const FbInfo& info)
{
    lock_.unwatch();

    stdlib::AutoLock lock(lock_);

    if (ready_flag_)
        return true;

    if (info.bpp != 32 && info.bpp != 24 && info.bpp != 16)
    {
        trace(0, "Fb: unsupported bpp %lu", (ulong)info.bpp);
        return false;
    }

    if (info.width < FONT8X16_WIDTH || info.height < FONT8X16_HEIGHT)
    {
        trace(0, "Fb: mode too small %lux%lu", (ulong)info.width, (ulong)info.height);
        return false;
    }

    if (info.phy_addr == 0 || (info.phy_addr & (consts::PAGE_SIZE - 1)) != 0)
    {
        trace(0, "Fb: unaligned framebuffer 0x%lX", info.phy_addr);
        return false;
    }

    if (info.pitch < (ulong)info.width * (info.bpp / 8))
    {
        trace(0, "Fb: bad pitch %lu for bpp %lu", (ulong)info.pitch, (ulong)info.bpp);
        return false;
    }

    /* Framebuffer memory has no read side effects, so it is mapped
       write-combining: stores get buffered and merged instead of going out
       one bus transaction at a time. Everything that draws ends with
       hal::wc_flush() so the pixels actually appear. */
    ulong size_bytes = (ulong)info.pitch * info.height;
    ulong va = mm::PageTable::get_instance().map_mmio_region(info.phy_addr, size_bytes,
        mm::PageTable::MmioWriteCombining);
    if (va == 0)
    {
        trace(0, "Fb: can't map framebuffer 0x%lX size %lu", info.phy_addr, size_bytes);
        return false;
    }

    info_ = info;
    fb_ = (u8 *)va;
    pitch_ = info.pitch;
    width_ = info.width;
    height_ = info.height;
    bytes_per_pixel_ = info.bpp / 8;
    fast_blit_ = ((pitch_ % sizeof(u64)) == 0);

    cols_ = width_ / FONT8X16_WIDTH;
    rows_ = height_ / FONT8X16_HEIGHT;
    if (cols_ > MAX_COLS)
        cols_ = MAX_COLS;
    if (rows_ > MAX_ROWS)
        rows_ = MAX_ROWS;

    fg_color_ = pack_color(0xAA, 0xAA, 0xAA);
    bg_color_ = pack_color(0x00, 0x00, 0x00);

    cls_lock_held();
    draw_cursor(true);
    hal::wc_flush();

    ready_flag_ = true;

    trace(0, "Fb: %lux%lu bpp %lu pitch %lu phys 0x%lX va 0x%lX grid %lux%lu",
        (ulong)width_, (ulong)height_, (ulong)info.bpp, (ulong)pitch_,
        info.phy_addr, va, (ulong)cols_, (ulong)rows_);

    return true;
}

void FbTerm::cls()
{
    stdlib::AutoLock lock(lock_);

    if (fb_ == nullptr)
        return;

    cls_lock_held();
    draw_cursor(true);
    hal::wc_flush();
}

void FbTerm::v_printf(const char *fmt, va_list args)
{
    char str[256];

    if (stdlib::vsn_printf(str, sizeof(str), fmt, args) < 0)
        return;

    stdlib::AutoLock lock(lock_);

    if (fb_ == nullptr)
        return;

    puts_lock_held(str);
}

void FbTerm::printf(const char *fmt, ...)
{
    va_list args;

    va_start(args, fmt);
    v_printf(fmt, args);
    va_end(args);
}

void FbTerm::print_string(const char *s)
{
    stdlib::AutoLock lock(lock_);

    if (fb_ == nullptr)
        return;

    puts_lock_held(s);
}

void FbTerm::panic_print_string(const char *s)
{
    /*
     * Bypass the lock -- draw straight into the framebuffer.
     * Safe only in panic context with interrupts disabled.
     */
    if (fb_ == nullptr)
        return;

    puts_lock_held(s);
}

void FbTerm::backspace()
{
    stdlib::AutoLock lock(lock_);

    if (fb_ == nullptr)
        return;

    draw_cursor(false);

    if (column_ > 0)
    {
        column_--;
    }
    else if (row_ > 0)
    {
        row_--;
        column_ = cols_ - 1;
    }

    cells_[row_][column_] = '\0';
    if (shown_[row_][column_] != '\0')
    {
        draw_glyph(row_, column_, '\0');
        shown_[row_][column_] = '\0';
    }

    draw_cursor(true);
    hal::wc_flush();
}

}
