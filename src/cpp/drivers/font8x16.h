#pragma once

#include <include/types.h>

namespace kernel
{

/* Monochrome 8x16 bitmap font for the framebuffer console: one byte per
   pixel row, bit 7 is the leftmost pixel. Covers printable ASCII
   0x20..0x7E; see scripts/gen-font8x16.py. */
const u32 FONT8X16_WIDTH = 8;
const u32 FONT8X16_HEIGHT = 16;
const char FONT8X16_FIRST_CHAR = 0x20;
const char FONT8X16_LAST_CHAR = 0x7E;
const u32 FONT8X16_GLYPH_COUNT = FONT8X16_LAST_CHAR - FONT8X16_FIRST_CHAR + 1;

extern const u8 FONT8X16[FONT8X16_GLYPH_COUNT][FONT8X16_HEIGHT];

}
