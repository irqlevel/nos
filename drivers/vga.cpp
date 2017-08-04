#include "vga.h"
#include <kernel/asm.h>
#include <lib/stdlib.h>
#include <mm/page_table.h>

namespace Kernel
{

VgaTerm::VgaTerm()
    : Buf(reinterpret_cast<u16*>(Mm::PageTable::GetInstance().PhysToVirt(BufAddr)))
    , Row(0)
    , Column(0)
    , Width(MaxWidth)
    , Height(MaxHeight)
    , ColorCode(MakeColor(ColorWhite, ColorBlack))
{
	Stdlib::AutoLock lock(Lock);

	ClsLockHeld();
    Cursor();
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
	Stdlib::AutoLock lock(Lock);

    ColorCode = MakeColor(fg, bg);
}

size_t VgaTerm::GetIndex(u8 x, u8 y)
{
	return y * Width + x;
}

void VgaTerm::PutCharAt(char c, u8 color, u8 x, u8 y)
{
	Buf[GetIndex(x, y)] = MakeEntry(c, color);
}

void VgaTerm::Overflow()
{
	if (Column == Width)
    {
		Column = 0;
		Row++;
	}

	if (Row == Height)
	{
		for (size_t row = 0; row < Height - 1; row++)
		{
			for (size_t column = 0; column < Width; column++)
			{
				Buf[GetIndex(column, row)] = Buf[GetIndex(column, row + 1)];
			}
		}

		for (size_t column = 0; column < Width; column++)
			PutCharAt('\0', MakeColor(ColorBlack, ColorBlack), column, Height - 1);

		Row = Height - 1;
	}
}

void VgaTerm::PutChar(char c)
{
	if (c == '\n')
	{
		while (Column < Width)
				PutCharAt(' ', ColorCode, Column++, Row);

		Column = 0;
		Row++;
		Overflow();
	}
	else
	{
	    PutCharAt(c, ColorCode, Column++, Row);
		Overflow();
    }

	Cursor();
}

void VgaTerm::PutsLockHeld(const char *str)
{
	for (;;)
	{
		char c = *str++;
		if (c == '\0')
			break;

		PutChar(c);
	}
}

void VgaTerm::Puts(const char *str)
{
	Stdlib::AutoLock lock(Lock);

	PutsLockHeld(str);
}

void VgaTerm::ClsLockHeld()
{
	for (u8 x = 0; x < Width; x++)
		for (u8 y = 0; y < Height; y++)
			PutCharAt('\0', MakeColor(ColorBlack, ColorBlack), x, y);
}

void VgaTerm::Cls()
{
	Stdlib::AutoLock lock(Lock);

	ClsLockHeld();
	Row = 0;
	Column = 0;
	Cursor();
}

void VgaTerm::VPrintf(const char *fmt, va_list args)
{
	char str[256];

	if (Stdlib::VsnPrintf(str, sizeof(str), fmt, args) < 0)
		return;

	Stdlib::AutoLock lock(Lock);

	PutsLockHeld(str);
}

void VgaTerm::Printf(const char *fmt, ...)
{
	va_list args;

	va_start(args, fmt);
	VPrintf(fmt, args);
	va_end(args);
}

void VgaTerm::PrintString(const char *s)
{
	Stdlib::AutoLock lock(Lock);

	PutsLockHeld(s);
}

void VgaTerm::Cursor()
{
	u16 offset = ((Row % Height) * Width + (Column % Width)) % (Width * Height);
	Outb(VgaBase, VgaIndex + 1);
	Outb(VgaBase + 1, offset & 0xFF);
	Outb(VgaBase, VgaIndex);
	Outb(VgaBase + 1, offset >> 8);
}

}
