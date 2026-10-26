#pragma once

#include <include/types.h>
#include <lib/stdlib.h>

namespace kernel
{

/* The video console, whatever the firmware left us with: a pixel
   framebuffer (UEFI GOP, or VBE when GRUB sets a graphics mode) drawn by
   FbTerm, or legacy EGA text at 0xB8000 driven by VgaTerm. On UEFI there
   is no text mode at all, so the framebuffer path is the only one that
   produces visible output.

   setup() picks the backend from what the bootloader reported; every
   other entry point is a no-op until then, and stays a no-op if no
   usable screen exists (serial still works). */
namespace screen
{

bool setup();

bool is_ready();

void print_string(const char *s);
PRINTF_FORMAT(1, 2) void printf(const char *fmt, ...);
void backspace();
void cls();

/* Panic context: no locks, polled output only. */
void panic_print_string(const char *s);

}

}
