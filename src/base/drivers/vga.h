#pragma once

#include <include/types.h>
#include <kernel/spin_lock.h>
#include <lib/printer.h>

namespace kernel
{

class VgaTerm : public stdlib::Printer
{
public:
    static VgaTerm& get_instance()
    {
        static VgaTerm instance;
        return instance;
    }

    static bool is_ready() { return ready_flag_; }

    void puts(const char *s);
    void cls();

    virtual PRINTF_FORMAT(2, 0) void v_printf(const char *fmt, va_list args) override;
    virtual PRINTF_FORMAT(2, 3) void printf(const char *fmt, ...) override;
    virtual void print_string(const char *s) override;
    virtual void backspace() override;

    void panic_print_string(const char *s);

    /* VGA hardware text mode color constants. */
    enum Color {
        ColorBlack = 0,
        ColorBlue = 1,
        ColorGreen = 2,
        ColorCyan = 3,
        ColorRed = 4,
        ColorMagenta = 5,
        ColorBrown = 6,
        ColorLightGray = 7,
        ColorDarkGray = 8,
        ColorLightBlue = 9,
        ColorLightGreen = 10,
        ColorLightCyan = 11,
        ColorLightRed = 12,
        ColorLightMagenta = 13,
        ColorLightBrown = 14,
        ColorWhite = 15,
    };

    void set_color(Color fg, Color bg);

private:
    VgaTerm();
    virtual ~VgaTerm();

    VgaTerm(const VgaTerm& other) = delete;
    VgaTerm(VgaTerm&& other) = delete;
    VgaTerm& operator=(const VgaTerm& other) = delete;
    VgaTerm& operator=(VgaTerm&& other) = delete;

    u8 make_color(Color fg, Color bg);
    u16 make_entry(char c, u8 color);

    void put_char_at(char c, u8 color, u8 x, u8 y);

    size_t get_index(u8 x, u8 y);

    void overflow();

    void put_char(char c);
    void cursor();

    void puts_lock_held(const char *s);
    void cls_lock_held();

    const ulong buf_phy_addr_ = 0xB8000;

    static const u8 MAX_WIDTH = 80;
    static const u8 MAX_HEIGHT = 25;

    u16 *buf_;
    u8 row_;
    u8 column_;
    u8 width_;
    u8 height_;
    u8 color_code_;

    const u16 vga_base_ = 0x3D4;
    const u16 vga_index_ = 0x0E;

    SpinLock lock_;

    static bool ready_flag_;
};

}
