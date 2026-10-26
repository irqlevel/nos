#include "serial.h"
#include <arch/x86_64/pic.h>
#include <hal/irqchip.h>

#include <arch/x86_64/asm.h>
#include <arch/x86_64/idt.h>
#include <kernel/interrupt.h>
#include <lib/stdlib.h>

namespace kernel
{

Serial::Serial()
    : present_(false)
    , int_vector_(-1)
{
    for (size_t i = 0; i < MAX_OBSERVER; i++)
        rx_observer_[i] = nullptr;

    /* Scratch-register probe before anything is configured: a 16550 holds
       whatever is written to register 7, an undecoded port reads back 0xFF
       (or 0x00 when something else answers). Laptops that expose no UART
       fail this, and every serial write below is then skipped -- otherwise
       Wait() burns its full ~1M pause budget per character on a port whose
       transmitter never reports empty. */
    outb(PORT + 7, 0xAE);
    present_ = (inb(PORT + 7) == 0xAE);

    if (!present_)
        return;

    outb(PORT + 1, 0x00);    // Disable all interrupts
    outb(PORT + 3, 0x80);    // Enable DLAB (set baud rate divisor)
    outb(PORT + 0, 0x01);    // Set divisor to 1 (lo byte) 115200 baud
    outb(PORT + 1, 0x00);    //                  (hi byte)
    outb(PORT + 3, 0x03);    // 8 bits, no parity, one stop bit
    outb(PORT + 2, 0xC7);    // Enable FIFO, clear them, with 14-byte threshold
    outb(PORT + 4, 0x0B);    // IRQs enabled, RTS/DSR set
}

bool Serial::is_present()
{
    return present_;
}

void Serial::send()
{
    ulong flags;

    if (!present_)
        return;

    for (;;)
    {
        lock_.lock(flags);

        if (!buf_.is_empty())
        {
            lock_.unlock(flags);
            wait();
            lock_.lock(flags);
            if (!buf_.is_empty())
                outb(PORT, buf_.get());
            lock_.unlock(flags);
        }
        else
        {
            lock_.unlock(flags);
            break;
        }
    }
}

void Serial::flush()
{
    send();
}

void Serial::wait()
{
    size_t pause_count = 1;

    /* Bounded: a stuck transmitter must not hang the kernel; give up
       and let the caller drop the character (~1M pauses total) */
    for (size_t round = 0; round < 20; round++)
    {
        if (is_transmit_empty())
            break;

        for (size_t i = 0; i < pause_count; i++)
            cpu_relax();

        pause_count *= 2;
    }
}

bool Serial::is_transmit_empty()
{
    return (inb(PORT + 5) & 0x20) ? true : false;
}

Serial::~Serial()
{
    send();
}

void Serial::print_string(const char *str)
{
    if (!present_)
        return;

    send();

    ulong flags;
    lock_.lock(flags);
    for (;;)
    {
        char c = *str++;
        if (c == '\0')
        {
            break;
        }

        if (c == '\n')
        {
            if (buf_.is_full())
            {
                lock_.unlock(flags);
                send();
                lock_.lock(flags);
            }
            buf_.put('\r');
        }

        if (buf_.is_full())
        {
            lock_.unlock(flags);
            send();
            lock_.lock(flags);
        }
        buf_.put(c);
    }
    lock_.unlock(flags);

    send();
}

void Serial::panic_print_string(const char *str)
{
    /*
     * Bypass lock and ring buffer — poll-write directly to UART.
     * Safe only in panic context with interrupts disabled.
     */
    if (!present_)
        return;

    while (*str)
    {
        char c = *str++;
        if (c == '\n')
        {
            while (!is_transmit_empty())
                cpu_relax();
            outb(PORT, '\r');
        }
        while (!is_transmit_empty())
            cpu_relax();
        outb(PORT, c);
    }
}

void Serial::v_printf(const char *fmt, va_list args)
{
	char str[256];

	if (stdlib::vsn_printf(str, sizeof(str), fmt, args) < 0)
		return;

	print_string(str);
}

void Serial::printf(const char *fmt, ...)
{
	va_list args;

	va_start(args, fmt);
	v_printf(fmt, args);
	va_end(args);
}

void Serial::backspace()
{
    /* Send BS, space, BS to erase character on serial terminal */
    print_string("\b \b");
}

bool Serial::register_observer(SerialObserver& observer)
{
    ulong flags;
    lock_.lock(flags);

    for (size_t i = 0; i < MAX_OBSERVER; i++)
    {
        if (rx_observer_[i] == nullptr)
        {
            rx_observer_[i] = &observer;
            lock_.unlock(flags);
            return true;
        }
    }

    lock_.unlock(flags);
    return false;
}

void Serial::unregister_observer(SerialObserver& observer)
{
    ulong flags;
    lock_.lock(flags);

    for (size_t i = 0; i < MAX_OBSERVER; i++)
    {
        if (rx_observer_[i] == &observer)
        {
            rx_observer_[i] = nullptr;
        }
    }

    lock_.unlock(flags);
}

void Serial::on_interrupt_register(u8 irq, u8 vector)
{
    (void)irq;

    int_vector_ = vector;

    /* Enable Received Data Available interrupt (IER bit 0) */
    outb(PORT + 1, 0x01);
}

InterruptHandlerFn Serial::get_handler_fn()
{
    return serial_interrupt_stub;
}

void Serial::interrupt(Context* ctx)
{
    (void)ctx;

    /* Check for received data (LSR bit 0 = Data Ready) */
    while (inb(PORT + 5) & 0x01)
    {
        u8 data = inb(PORT);
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

        for (size_t i = 0; i < MAX_OBSERVER; i++)
        {
            if (rx_observer_[i] != nullptr)
                rx_observer_[i]->on_char(c, code);
        }
    }

    send();

    hal::irq_eoi(int_vector_);
}

extern "C" void serial_interrupt(Context* ctx)
{
    InterruptStats::inc(IrqSerial);
    Serial::get_instance().interrupt(ctx);
}

}