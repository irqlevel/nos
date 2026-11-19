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

/* A function's BARs about to be given to a guest: drawing held (on) and
   taken up again (off), paired, while they are sized with its decoding off;
   then the framebuffer let go of for good where it lies in one of them,
   [base, base + size) -- true when it did (FbTerm). EGA text is at the
   VGA's own addresses, no BAR's, and is neither held nor let go of. */
void hold(bool on);
bool release_within(ulong base, ulong size);

}

}
