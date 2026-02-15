#include "serial.h"
#include "pic.h"
#include "lapic.h"

#include <kernel/asm.h>
#include <kernel/idt.h>
#include <kernel/interrupt.h>
#include <lib/stdlib.h>

namespace Kernel
{

Serial::Serial()
    : IntVector(-1)
{
    for (size_t i = 0; i < MaxObserver; i++)
        RxObserver[i] = nullptr;

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

        if (c == '\n')
        {
            if (Buf.IsFull())
            {
                Lock.Unlock(flags);
                Send();
                Lock.Lock(flags);
            }
            Buf.Put('\r');
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

    Send();
}

void Serial::PanicPrintString(const char *str)
{
    /*
     * Bypass lock and ring buffer — poll-write directly to UART.
     * Safe only in panic context with interrupts disabled.
     */
    while (*str)
    {
        char c = *str++;
        if (c == '\n')
        {
            while (!IsTransmitEmpty())
                Pause();
            Outb(Port, '\r');
        }
        while (!IsTransmitEmpty())
            Pause();
        Outb(Port, c);
    }
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

void Serial::Backspace()
{
    /* Send BS, space, BS to erase character on serial terminal */
    PrintString("\b \b");
}

bool Serial::RegisterObserver(SerialObserver& observer)
{
    ulong flags;
    Lock.Lock(flags);

    for (size_t i = 0; i < MaxObserver; i++)
    {
        if (RxObserver[i] == nullptr)
        {
            RxObserver[i] = &observer;
            Lock.Unlock(flags);
            return true;
        }
    }

    Lock.Unlock(flags);
    return false;
}

void Serial::UnregisterObserver(SerialObserver& observer)
{
    ulong flags;
    Lock.Lock(flags);

    for (size_t i = 0; i < MaxObserver; i++)
    {
        if (RxObserver[i] == &observer)
        {
            RxObserver[i] = nullptr;
        }
    }

    Lock.Unlock(flags);
}

void Serial::OnInterruptRegister(u8 irq, u8 vector)
{
    (void)irq;

    IntVector = vector;

    /* Enable Received Data Available interrupt (IER bit 0) */
    Outb(Port + 1, 0x01);
}

InterruptHandlerFn Serial::GetHandlerFn()
{
    return SerialInterruptStub;
}

void Serial::Interrupt(Context* ctx)
{
    (void)ctx;

    /* Check for received data (LSR bit 0 = Data Ready) */
    while (Inb(Port + 5) & 0x01)
    {
        u8 data = Inb(Port);
        char c = (char)data;
        u8 code = 0;

        if (c == '\r')
        {
            c = '\n';
        }
        else if (c == 0x7F || c == 0x08) /* DEL or BS */
        {
            c = '\b';
            code = 0x0E; /* PS/2 backspace scan code */
        }

        for (size_t i = 0; i < MaxObserver; i++)
        {
            if (RxObserver[i] != nullptr)
                RxObserver[i]->OnChar(c, code);
        }
    }

    Send();

    Lapic::EOI(IntVector);
}

extern "C" void SerialInterrupt(Context* ctx)
{
    InterruptStats::Inc(IrqSerial);
    Serial::GetInstance().Interrupt(ctx);
}

}