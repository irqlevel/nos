#include "vga.h"
#include "stdlib.h"

namespace Kernel
{

namespace Core
{

VgaTerm::VgaTerm()
    : Buf(reinterpret_cast<u16*>(BufAddr))
    , Row(0)
    , Column(0)
    , Width(80)
    , Height(25)
    , ColorCode(MakeColor(ColorWhite, ColorBlack))
{
	Cls();
}

VgaTerm::~VgaTerm()
{
}

u8 VgaTerm::MakeColor(Color fg, Color bg)
{
    return fg | bg << 4;
}

u16 VgaTerm::MakeEntry(char c, u8 color)
{
	return (u16)c | (u16)color << 8;
}

void VgaTerm::SetColor(Color fg, Color bg)
{
    ColorCode = MakeColor(fg, bg);
}

void VgaTerm::PutCharAt(char c, u8 color, u8 x, u8 y)
{
	const size_t index = y * Width + x;

	Buf[index] = MakeEntry(c, color);
}

void VgaTerm::Overflow()
{
	if (Column == Width)
    {
		Column = 0;
		Row++;
	}

	if (Row == Height)
		Row = 0;
}

void VgaTerm::PutChar(char c)
{
	if (c == '\n') {
		Column = 0;
		Row++;
		Overflow();
		return;
	}

	PutCharAt(c, ColorCode, Column, Row);
	Column++;
	Overflow();
}

void VgaTerm::Puts(const char *str)
{
	size_t len = Shared::StrLen(str);

	for (size_t i = 0; i < len; i++)
		PutChar(str[i]);
}

void VgaTerm::Cls()
{
	for (u8 x = 0; x < Width; x++)
		for (u8 y = 0; y < Height; y++)
			PutCharAt('\0', MakeColor(ColorBlack, ColorBlack), x, y);
}

void VgaTerm::Vprintf(const char *fmt, va_list args)
{
	char str[256];

	if (Shared::VsnPrintf(str, sizeof(str), fmt, args) < 0)
		return;

	Puts(str);
}

void VgaTerm::Printf(const char *fmt, ...)
{
	va_list args;

	va_start(args, fmt);
	Vprintf(fmt, args);
	va_end(args);
}

}
}