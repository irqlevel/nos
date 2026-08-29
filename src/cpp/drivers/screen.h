#pragma once

#include <include/types.h>
#include <lib/stdlib.h>

namespace Kernel
{

/* The video console, whatever the firmware left us with: a pixel
   framebuffer (UEFI GOP, or VBE when GRUB sets a graphics mode) drawn by
   FbTerm, or legacy EGA text at 0xB8000 driven by VgaTerm. On UEFI there
   is no text mode at all, so the framebuffer path is the only one that
   produces visible output.

   Setup() picks the backend from what the bootloader reported; every
   other entry point is a no-op until then, and stays a no-op if no
   usable screen exists (serial still works). */
namespace Screen
{

bool Setup();

bool IsReady();

void PrintString(const char *s);
void Printf(const char *fmt, ...);
void Backspace();
void Cls();

/* Panic context: no locks, polled output only. */
void PanicPrintString(const char *s);

}

}
