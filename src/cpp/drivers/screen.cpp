#include "screen.h"
#include "fb_console.h"
#include "vga.h"

#include <kernel/trace.h>
#include <arch/x86_64/grub.h>

namespace kernel
{

namespace screen
{

/* Which backend setup() selected; false means VgaTerm (or nothing). */
static bool fb_active;

bool setup()
{
    const grub::FramebufferInfo *fb = grub::get_framebuffer_info();

    if (fb != nullptr && fb->type == grub::MULTI_BOOT_FRAMEBUFFER_TYPE_RGB)
    {
        FbInfo info;

        info.phy_addr = (ulong)fb->addr;
        info.pitch = fb->pitch;
        info.width = fb->width;
        info.height = fb->height;
        info.bpp = fb->bpp;
        info.red_pos = fb->red_pos;
        info.red_size = fb->red_size;
        info.green_pos = fb->green_pos;
        info.green_size = fb->green_size;
        info.blue_pos = fb->blue_pos;
        info.blue_size = fb->blue_size;

        if (FbTerm::get_instance().setup(info))
        {
            fb_active = true;
            return true;
        }

        trace(0, "Screen: framebuffer console setup failed");
        return false;
    }

    if (fb != nullptr && fb->type != grub::MULTI_BOOT_FRAMEBUFFER_TYPE_EGA_TEXT)
    {
        /* Indexed-color (palette) modes are not supported: there is no
           text mode to fall back to either, so the screen stays dark. */
        trace(0, "Screen: unsupported framebuffer type %lu, no video console",
            (ulong)fb->type);
        return false;
    }

    /* EGA text, or a bootloader that reported no framebuffer at all --
       VgaTerm decides for itself whether 0xB8000 is real hardware. */
    VgaTerm::get_instance();

    if (!VgaTerm::is_ready())
        trace(0, "Screen: no video console (serial only)");

    return VgaTerm::is_ready();
}

bool is_ready()
{
    return fb_active ? FbTerm::is_ready() : VgaTerm::is_ready();
}

void print_string(const char *s)
{
    if (fb_active)
        FbTerm::get_instance().print_string(s);
    else
        VgaTerm::get_instance().print_string(s);
}

void printf(const char *fmt, ...)
{
    va_list args;

    va_start(args, fmt);
    if (fb_active)
        FbTerm::get_instance().v_printf(fmt, args);
    else
        VgaTerm::get_instance().v_printf(fmt, args);
    va_end(args);
}

void backspace()
{
    if (fb_active)
        FbTerm::get_instance().backspace();
    else
        VgaTerm::get_instance().backspace();
}

void cls()
{
    if (fb_active)
        FbTerm::get_instance().cls();
    else
        VgaTerm::get_instance().cls();
}

void panic_print_string(const char *s)
{
    if (fb_active)
        FbTerm::get_instance().panic_print_string(s);
    else
        VgaTerm::get_instance().panic_print_string(s);
}

}

}
