#include "serial.h"
#include "pic.h"
#include "lapic.h"

#include <kernel/asm.h>
#include <kernel/idt.h>
#include <lib/stdlib.h>

namespace Kernel
{

namespace Core
{

Serial::Serial()
    : IntVector(-1)
{
    Outb(Port + 1, 0x00);    // Disable all interrupts
    Outb(Port + 3, 0x80);    // Enable DLAB (set baud rate divisor)
    Outb(Port + 0, 0x01);    // Set divisor to 1 (lo byte) 115200 baud
    Outb(Port + 1, 0x00);    //                  (hi byte)
    Outb(Port + 3, 0x03);    // 8 bits, no parity, one stop bit
    Outb(Port + 2, 0xC7);    // Enable FIFO, clear them, with 14-byte threshold
    Outb(Port + 4, 0x0B);    // IRQs enabled, RTS/DSR set
}

void Serial::Send()
{
    while (!Buf.IsEmpty() && IsTransmitEmpty())
    {
        Outb(Port, Buf.Get());
    }
}

void Serial::Wait(size_t pauseCount)
{
    for (size_t i = 0; i < pauseCount; i++)
    {
        if (IsTransmitEmpty())
            break;

        Pause();
    }
}

void Serial::WriteChar(char c)
{
    size_t pauseCount = 1;

again:
    if (Buf.IsFull())
        Send();

    if (!Buf.Put(c))
    {
        Wait(pauseCount);
        pauseCount *= 2;
        goto again;
    }

    Send();
}

bool Serial::IsTransmitEmpty()
{
    return (Inb(Port + 5) & 0x20) ? true : false;
}

Serial::~Serial()
{
}

void Serial::PrintString(const char *str)
{
    Shared::AutoLock lock(Lock);

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

	PrintString(str);
}

void Serial::Printf(const char *fmt, ...)
{
	va_list args;

	va_start(args, fmt);
	Vprintf(fmt, args);
	va_end(args);
}

void Serial::OnInterruptRegister(u8 irq, u8 vector)
{
    (void)irq;

    IntVector = vector;
}

InterruptHandlerFn Serial::GetHandlerFn()
{
    return IO8042InterruptStub;
}

void Serial::Interrupt(Context* ctx)
{
    (void)ctx;

    Shared::AutoLock lock(Lock);
    Send();
    Lapic::EOI(IntVector);
}

extern "C" void SerialInterrupt(Context* ctx)
{
    Serial::GetInstance().Interrupt(ctx);
}

}
}