#include "serial.h"
#include "pic.h"
#include "lapic.h"

#include <kernel/asm.h>
#include <kernel/idt.h>
#include <lib/stdlib.h>

namespace Kernel
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
    ulong flags;

    for (;;)
    {
        Lock.Lock(flags);

        if (!Buf.IsEmpty())
        {
            Lock.Unlock(flags);
            Wait();
            Lock.Lock(flags);
            if (!Buf.IsEmpty())
                Outb(Port, Buf.Get());
            Lock.Unlock(flags);
        }
        else
        {
            Lock.Unlock(flags);
            break;
        }
    }
}

void Serial::Flush()
{
    Send();
}

void Serial::Wait()
{
    size_t pauseCount = 1;

    for (;;)
    {
        if (IsTransmitEmpty())
            break;

        for (size_t i = 0; i < pauseCount; i++)
            Pause();

        pauseCount *= 2;
    }
}

bool Serial::IsTransmitEmpty()
{
    return (Inb(Port + 5) & 0x20) ? true : false;
}

Serial::~Serial()
{
    Send();
}

void Serial::PrintString(const char *str)
{
    Send();

    ulong flags;
    Lock.Lock(flags);
    for (;;)
    {
        char c = *str++;
        if (c == '\0')
        {
            break;
        }

        if (Buf.IsFull())
        {
            Lock.Unlock(flags);
            Send();
            Lock.Lock(flags);
        }

        Buf.Put(c);
    }
    Lock.Unlock(flags);
}

void Serial::VPrintf(const char *fmt, va_list args)
{
	char str[256];

	if (Stdlib::VsnPrintf(str, sizeof(str), fmt, args) < 0)
		return;

	PrintString(str);
}

void Serial::Printf(const char *fmt, ...)
{
	va_list args;

	va_start(args, fmt);
	VPrintf(fmt, args);
	va_end(args);
}

void Serial::OnInterruptRegister(u8 irq, u8 vector)
{
    (void)irq;

    IntVector = vector;
}

InterruptHandlerFn Serial::GetHandlerFn()
{
    return SerialInterruptStub;
}

void Serial::Interrupt(Context* ctx)
{
    (void)ctx;

    Send();

    Lapic::EOI(IntVector);
}

extern "C" void SerialInterrupt(Context* ctx)
{
    Serial::GetInstance().Interrupt(ctx);
}

}