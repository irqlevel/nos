#include "screen.h"
#include "fb_console.h"
#include "vga.h"

#include <kernel/trace.h>
#include <arch/x86_64/grub.h>

namespace Kernel
{

namespace Screen
{

/* Which backend Setup() selected; false means VgaTerm (or nothing). */
static bool FbActive;

bool Setup()
{
    const Grub::FramebufferInfo *fb = Grub::GetFramebufferInfo();

    if (fb != nullptr && fb->Type == Grub::MultiBootFramebufferTypeRgb)
    {
        FbInfo info;

        info.PhyAddr = (ulong)fb->Addr;
        info.Pitch = fb->Pitch;
        info.Width = fb->Width;
        info.Height = fb->Height;
        info.Bpp = fb->Bpp;
        info.RedPos = fb->RedPos;
        info.RedSize = fb->RedSize;
        info.GreenPos = fb->GreenPos;
        info.GreenSize = fb->GreenSize;
        info.BluePos = fb->BluePos;
        info.BlueSize = fb->BlueSize;

        if (FbTerm::GetInstance().Setup(info))
        {
            FbActive = true;
            return true;
        }

        Trace(0, "Screen: framebuffer console setup failed");
        return false;
    }

    if (fb != nullptr && fb->Type != Grub::MultiBootFramebufferTypeEgaText)
    {
        /* Indexed-color (palette) modes are not supported: there is no
           text mode to fall back to either, so the screen stays dark. */
        Trace(0, "Screen: unsupported framebuffer type %u, no video console",
            (ulong)fb->Type);
        return false;
    }

    /* EGA text, or a bootloader that reported no framebuffer at all --
       VgaTerm decides for itself whether 0xB8000 is real hardware. */
    VgaTerm::GetInstance();

    if (!VgaTerm::IsReady())
        Trace(0, "Screen: no video console (serial only)");

    return VgaTerm::IsReady();
}

bool IsReady()
{
    return FbActive ? FbTerm::IsReady() : VgaTerm::IsReady();
}

void PrintString(const char *s)
{
    if (FbActive)
        FbTerm::GetInstance().PrintString(s);
    else
        VgaTerm::GetInstance().PrintString(s);
}

void Printf(const char *fmt, ...)
{
    va_list args;

    va_start(args, fmt);
    if (FbActive)
        FbTerm::GetInstance().VPrintf(fmt, args);
    else
        VgaTerm::GetInstance().VPrintf(fmt, args);
    va_end(args);
}

void Backspace()
{
    if (FbActive)
        FbTerm::GetInstance().Backspace();
    else
        VgaTerm::GetInstance().Backspace();
}

void Cls()
{
    if (FbActive)
        FbTerm::GetInstance().Cls();
    else
        VgaTerm::GetInstance().Cls();
}

void PanicPrintString(const char *s)
{
    if (FbActive)
        FbTerm::GetInstance().PanicPrintString(s);
    else
        VgaTerm::GetInstance().PanicPrintString(s);
}

}

}
