#include "serial.h"
#include "helpers32.h"
#include "stdlib.h"
#include "idt.h"
#include "pic.h"

namespace Kernel
{

namespace Core
{

Serial::Serial()
    : IntNum(-1)
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
        hlt();
    }
}

Serial::~Serial()
{
    Wait();
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

void Serial::RegisterInterrupt(int intNum)
{
    auto& idt = Idt::GetInstance();

    idt.SetDescriptor(intNum, IdtDescriptor::Encode(IO8042InterruptStub));
    IntNum = intNum;
}

void Serial::UnregisterInterrupt()
{
    if (IntNum >= 0)
    {
        auto& idt = Idt::GetInstance();

        idt.SetDescriptor(IntNum, IdtDescriptor(0));
        IntNum = -1;
    }
}

void Serial::OnInterrupt()
{
}

void Serial::Interrupt()
{
    auto& serial = Serial::GetInstance();

    serial.OnInterrupt();
    Pic::EOI();
}

extern "C" void SerialInterrupt()
{
    Serial::Interrupt();
}

}
}