#include "serial.h"
#include "helpers32.h"
#include "stdlib.h"

namespace Kernel
{

namespace Core
{

Serial::Serial()
{
    outb(Port + 1, 0x00);    // Disable all interrupts
    outb(Port + 3, 0x80);    // Enable DLAB (set baud rate divisor)
    outb(Port + 0, 0x03);    // Set divisor to 3 (lo byte) 38400 baud
    outb(Port + 1, 0x00);    //                  (hi byte)
    outb(Port + 3, 0x03);    // 8 bits, no parity, one stop bit
    outb(Port + 2, 0xC7);    // Enable FIFO, clear them, with 14-byte threshold
    outb(Port + 4, 0x0B);    // IRQs enabled, RTS/DSR set
}

void Serial::WriteChar(char c)
{
    Wait();

    outb(Port, c);
}

bool Serial::IsTransmitEmpty()
{
    return (inb(Port + 5) & 0x20) ? true : false;
}

void Serial::Wait()
{
    while (!IsTransmitEmpty())
    {
        pause_32();
    }
}

Serial::~Serial()
{
}

void Serial::WriteString(const char *str)
{
    for (;;)
    {
        char c = *str++;
        if (c == '\0')
            break;

        WriteChar(c);
    }
}

void Serial::Vprintf(const char *fmt, va_list args)
{
	char str[256];

	if (Shared::VsnPrintf(str, sizeof(str), fmt, args) < 0)
		return;

	WriteString(str);
}

void Serial::Printf(const char *fmt, ...)
{
	va_list args;

	va_start(args, fmt);
	Vprintf(fmt, args);
	va_end(args);
}

}
}