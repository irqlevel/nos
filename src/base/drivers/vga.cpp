#include "vga.h"
#include <arch/x86_64/asm.h>
#include <kernel/watchdog.h>
#include <kernel/klog.h>
#include <lib/stdlib.h>
#include <mm/page_table.h>
#include <arch/x86_64/grub.h>

namespace kernel
{

bool VgaTerm::ready_flag_ = false;

VgaTerm::VgaTerm()
    : buf_(nullptr)
    , row_(0)
    , column_(0)
    , width_(MAX_WIDTH)
    , height_(MAX_HEIGHT)
    , color_code_(make_color(ColorWhite, ColorBlack))
{
    lock_.unwatch();

    stdlib::AutoLock lock(lock_);

    /* On UEFI there is no EGA text mode: GRUB reports a pixel
       framebuffer instead. Writing to 0xB8000 would scribble over
       plain RAM, so leave the terminal disabled (ready_flag_ stays
       false, Buf stays null and all output methods are no-ops). */
    if (grub::has_framebuffer_info() && !grub::is_framebuffer_ega_text())
    {
        klog(0, "Vga: no EGA text mode (pixel framebuffer), output disabled");
        return;
    }

    buf_ = (u16*)mm::PageTable::get_instance().tmp_map_page(buf_phy_addr_);
    if (buf_ == nullptr)
    {
        PANIC("Can't map page");
    }

    klog(0, "Vga 0x%p buf 0x%lX mapped at 0x%p", this, buf_phy_addr_, buf_);

    /* Set cursor shape to underline (scan lines 13-15) */
    outb(vga_base_, 0x0A);
    outb(vga_base_ + 1, 0x0D);
    outb(vga_base_, 0x0B);
    outb(vga_base_ + 1, 0x0F);

    cls_lock_held();
    cursor(Cursor{ row_, column_ });

    ready_flag_ = true;
}

VgaTerm::~VgaTerm()
{
    klog(0, "Vga 0x%p dtor", this);

    stdlib::AutoLock lock(lock_);

    if (buf_ != nullptr)
    {
        mm::PageTable::get_instance().tmp_unmap_page((ulong)buf_);
        buf_ = nullptr;
    }
}

u8 VgaTerm::make_color(Color fg, Color bg)
{
    return fg | bg << 4;
}

u16 VgaTerm::make_entry(char c, u8 color)
{
    /* Through u8: char is signed here, and a byte of 0x80 or more -- a
       file's UTF-8 -- widened as it is would carry its sign into the
       attribute byte */
    return (u16)(u8)c | (u16)color << 8;
}

void VgaTerm::set_color(Color fg, Color bg)
{
    stdlib::AutoLock lock(lock_);

    color_code_ = make_color(fg, bg);
}

size_t VgaTerm::get_index(u8 x, u8 y)
{
	return y * width_ + x;
}

void VgaTerm::put_char_at(char c, u8 color, u8 x, u8 y)
{
    /* A cell off the screen is not written -- and not a BUG_ON: this is what
       a panic prints through, and a panic in it is one that only spins.
       The panic's path found the cursor there, another CPU being in the
       middle of a line feed, and stopped at its first character. */
    if (x >= width_ || y >= height_)
        return;

    buf_[get_index(x, y)] = make_entry(c, color);
}

/* A cursor a column or a row past the screen brought back onto it: the row
   past the last scrolls the screen up */
void VgaTerm::overflow(Cursor& at)
{
    if (at.column >= width_)
    {
        at.column = 0;
        at.row++;
    }

    if (at.row >= height_)
    {
        for (size_t row = 0; row < (size_t)(height_ - 1); row++)
        {
            for (size_t column = 0; column < width_; column++)
            {
                buf_[get_index(column, row)] = buf_[get_index(column, row + 1)];
            }
        }

        for (size_t column = 0; column < width_; column++)
            put_char_at('\0', make_color(ColorBlack, ColorBlack), column, height_ - 1);

        at.row = height_ - 1;
    }
}

/* `at` on the screen on the way in, and on the way out */
void VgaTerm::put_char(Cursor& at, char c)
{
    if (c == '\n')
    {
        while (at.column < width_)
            put_char_at('\0', color_code_, at.column++, at.row);

        at.column = 0;
        at.row++;
        overflow(at);
    }
    else
    {
        put_char_at(c, color_code_, at.column++, at.row);
        overflow(at);
    }

    cursor(at);
}

void VgaTerm::puts_from(Cursor at, const char *str)
{
    for (;;)
    {
        char c = *str++;
        if (c == '\0')
            break;

        put_char(at, c);
    }

    row_ = at.row;
    column_ = at.column;
}

void VgaTerm::puts_lock_held(const char *str)
{
    /* Under the lock the members are on the screen: every writer leaves
       them there before it lets go of it */
    puts_from(Cursor{ row_, column_ }, str);
}

void VgaTerm::puts(const char *str)
{
    stdlib::AutoLock lock(lock_);

    if (buf_ == nullptr)
        return;

    puts_lock_held(str);
}

void VgaTerm::cls_lock_held()
{
    for (u8 x = 0; x < width_; x++)
        for (u8 y = 0; y < height_; y++)
            put_char_at('\0', make_color(ColorBlack, ColorBlack), x, y);
}

void VgaTerm::cls()
{
    stdlib::AutoLock lock(lock_);

    if (buf_ == nullptr)
        return;

    cls_lock_held();
    row_ = 0;
    column_ = 0;
    cursor(Cursor{ row_, column_ });
}

void VgaTerm::v_printf(const char *fmt, va_list args)
{
    char str[256];

    if (stdlib::vsn_printf(str, sizeof(str), fmt, args) < 0)
        return;

    stdlib::AutoLock lock(lock_);

    if (buf_ == nullptr)
        return;

    puts_lock_held(str);
}

void VgaTerm::printf(const char *fmt, ...)
{
    va_list args;

    va_start(args, fmt);
    v_printf(fmt, args);
    va_end(args);
}

void VgaTerm::print_string(const char *s)
{
    stdlib::AutoLock lock(lock_);

    if (buf_ == nullptr)
        return;

    puts_lock_held(s);
}

void VgaTerm::panic_print_string(const char *s)
{
    /*
     * Bypass the lock -- write directly to the VGA buffer: whoever holds it
     * may be a CPU the panic has stopped, for good.
     *
     * So the cursor may be found mid-update -- a panic prints before it
     * stops the other CPUs, and one of them inside a line feed has row_ one
     * past the screen until it has scrolled -- and is read once and held to
     * the screen here (FbTerm::panic_print_string has the long of it).
     */
    if (buf_ == nullptr || width_ == 0 || height_ == 0)
        return;

    Cursor at = { row_, column_ };
    if (at.row >= height_)
        at.row = height_ - 1;
    if (at.column >= width_)
        at.column = 0;

    puts_from(at, s);
}

void VgaTerm::cursor(const Cursor& at)
{
    u16 offset = ((at.row % height_) * width_ + (at.column % width_)) % (width_ * height_);
    outb(vga_base_, vga_index_ + 1);
    outb(vga_base_ + 1, offset & 0xFF);
    outb(vga_base_, vga_index_);
    outb(vga_base_ + 1, offset >> 8);
}

void VgaTerm::backspace()
{
    stdlib::AutoLock lock(lock_);

    if (buf_ == nullptr)
        return;

    if (column_ > 0)
    {
        put_char_at('\0', make_color(ColorBlack, ColorBlack), --column_, row_);
    }
    else
    {
        if (row_ > 0)
        {
            --row_;
            column_ = width_ - 1;
            put_char_at('\0', make_color(ColorBlack, ColorBlack), column_, row_);
        }
    }

    cursor(Cursor{ row_, column_ });
}

}
