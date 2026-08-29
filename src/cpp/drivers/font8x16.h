#pragma once

#include <include/types.h>

namespace Kernel
{

/* Monochrome 8x16 bitmap font for the framebuffer console: one byte per
   pixel row, bit 7 is the leftmost pixel. Covers printable ASCII
   0x20..0x7E; see scripts/gen-font8x16.py. */
const u32 Font8x16Width = 8;
const u32 Font8x16Height = 16;
const char Font8x16FirstChar = 0x20;
const char Font8x16LastChar = 0x7E;
const u32 Font8x16GlyphCount = Font8x16LastChar - Font8x16FirstChar + 1;

extern const u8 Font8x16[Font8x16GlyphCount][Font8x16Height];

}
