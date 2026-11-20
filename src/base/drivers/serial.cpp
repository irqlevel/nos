#include "serial.h"
#include <arch/x86_64/pic.h>
#include <hal/irqchip.h>

#include <arch/x86_64/asm.h>
#include <arch/x86_64/idt.h>
#include <kernel/interrupt.h>
#include <kernel/tracepoint.h>
#include <lib/stdlib.h>

namespace kernel
{

namespace
{

/* The registers, at PORT plus these */
const int REG_DATA = 0;
const int REG_IER = 1;
const int REG_IIR = 2;      /* read */
const int REG_FCR = 2;      /* written */
const int REG_LSR = 5;
const int REG_MSR = 6;

const u8 IER_RX_DATA = 0x01;
/* The transmitter's holding register -- its FIFO, with one -- empty */
const u8 IER_TX_EMPTY = 0x02;

/* No interrupt pending, and what one is when there is */
const u8 IIR_NONE = 0x01;
const u8 IIR_ID_MASK = 0x0E;
const u8 IIR_ID_MODEM = 0x00;
const u8 IIR_ID_TX_EMPTY = 0x02;
const u8 IIR_ID_RX_DATA = 0x04;
const u8 IIR_ID_LINE = 0x06;
const u8 IIR_ID_RX_TIMEOUT = 0x0C;
/* Both set: the FIFOs FCR asked for are there, a 16550A's */
const u8 IIR_FIFOS = 0xC0;

const u8 LSR_DATA_READY = 0x01;
const u8 LSR_TX_EMPTY = 0x20;

/* A 16550A's transmit FIFO */
const size_t FIFO_BYTES = 16;

/* Interrupts one call of the handler takes, at most, and bytes received
   in one of them: a UART that never stops asking does not keep a CPU in it */
const ulong IRQ_ROUNDS_MAX = 64;
const ulong RX_DRAIN_MAX = 256;

/* What one pump sends at most, under a lock with interrupts off */
const size_t PUMP_MAX_BYTES = 4 * FIFO_BYTES;

/* The timer's look for an interrupt lost */
const ulong POLL_PERIOD_MS = 100;

/* What of the ring a panic sends before its report: its newest lines, the
   rest said and skipped -- at 115200 baud the whole ring is twenty seconds,
   and the netconsole's report and the reset wait on the console's */
const size_t PANIC_TAIL_BYTES = 2048;
/* And a halt's or a reset's: their own lines, and the ones before them */
const size_t HALT_TAIL_BYTES = 8192;

}

Serial::Serial()
    : present_(false)
    , int_vector_(-1)
    , fifo_depth_(1)
    , tx_irq_(false)
    , ier_(0)
    , panic_started_(false)
{
    for (size_t i = 0; i < MAX_OBSERVER; i++)
        rx_observer_[i] = nullptr;

    /* Scratch-register probe before anything is configured: a 16550 holds
       whatever is written to register 7, an undecoded port reads back 0xFF
       (or 0x00 when something else answers). Laptops that expose no UART
       fail this, and every serial write below is then skipped -- otherwise
       wait() burns its full ~1M pause budget per character on a port whose
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
    outb(PORT + REG_FCR, 0xC7);    // Enable FIFO, clear them, with 14-byte threshold
    outb(PORT + 4, 0x0B);    // IRQs enabled, RTS/DSR set

    /* The FIFOs are there when IIR says so; a UART without them takes a
       byte at a time */
    if ((inb(PORT + REG_IIR) & IIR_FIFOS) == IIR_FIFOS)
        fifo_depth_ = FIFO_BYTES;
}

bool Serial::is_present()
{
    return present_;
}

/* A line status register with every bit set: no UART's status, what a port
   nothing decodes reads as (interrupt) */
static const u8 LSR_NOTHING_THERE = 0xFF;

void Serial::pump_locked()
{
    /* The FIFO empty each round: on a real UART a FIFO's worth, the line
       taking a millisecond and more to send it; under QEMU, whose UART
       sends at once, up to PUMP_MAX_BYTES, the interrupt the rest -- not
       the whole ring with the lock held and interrupts off */
    size_t sent = 0;
    while (sent < PUMP_MAX_BYTES && !buf_.is_empty() && is_transmit_empty())
    {
        for (size_t i = 0; i < fifo_depth_ && !buf_.is_empty(); i++, sent++)
            outb(PORT + REG_DATA, buf_.get());
    }
}

void Serial::arm_locked()
{
    u8 want = static_cast<u8>((ier_ & ~IER_TX_EMPTY) | (buf_.is_empty() ? 0 : IER_TX_EMPTY));
    if (want != ier_)
    {
        ier_ = want;
        outb(PORT + REG_IER, ier_);
    }
}

void Serial::send()
{
    ulong flags;

    if (!present_)
        return;

    for (;;)
    {
        /* With the lock let go of: the wait is milliseconds at a time */
        bool empty_tx = wait();

        lock_.lock(flags);
        if (buf_.is_empty())
        {
            arm_locked();
            lock_.unlock(flags);
            break;
        }
        /* A transmitter that never says empty is written a byte anyway --
           whatever becomes of it, the ring moves on */
        size_t n = empty_tx ? fifo_depth_ : 1;
        for (size_t i = 0; i < n && !buf_.is_empty(); i++)
            outb(PORT + REG_DATA, buf_.get());
        lock_.unlock(flags);
    }
}

void Serial::flush()
{
    send();
}

void Serial::make_room()
{
    /* A FIFO's worth, waited for: the writer of a ring that filled goes on
       at the line's own pace, as every writer did before the ring */
    bool empty_tx = wait();

    ulong flags;
    lock_.lock(flags);
    size_t n = empty_tx ? fifo_depth_ : 1;
    for (size_t i = 0; i < n && !buf_.is_empty(); i++)
        outb(PORT + REG_DATA, buf_.get());
    lock_.unlock(flags);
}

void Serial::stop_tx_interrupt()
{
    if (!present_)
        return;

    ulong flags;
    lock_.lock(flags);
    tx_irq_ = false;
    ier_ = static_cast<u8>(ier_ & ~IER_TX_EMPTY);
    outb(PORT + REG_IER, ier_);

    /* The newest of what is waiting, the rest said and skipped: going down
       is no time to wait twenty seconds for a ring */
    size_t pending = buf_.get_size();
    size_t skipped = (pending > HALT_TAIL_BYTES) ? pending - HALT_TAIL_BYTES : 0;
    for (size_t i = 0; i < skipped; i++)
        buf_.pop_head();
    lock_.unlock(flags);

    if (skipped != 0)
    {
        char note[96];
        stdlib::sn_printf(note, sizeof(note), "\n[serial: %lu bytes before this not sent here]\n",
            static_cast<ulong>(skipped));
        print_string(note);
    }
    send();
}

void Serial::start_tx_interrupt()
{
    if (!present_)
        return;

    tx_irq_ = true;
    {
        ulong flags;
        lock_.lock(flags);
        pump_locked();
        arm_locked();
        lock_.unlock(flags);
    }
    (void)TimerTable::get_instance().start_timer(*this, stdlib::Time(POLL_PERIOD_MS * consts::NANO_SECS_IN_MS));
}

void Serial::on_tick(TimerCallback& callback)
{
    (void)callback;

    if (!present_ || !tx_irq_)
        return;

    /* Whoever holds the lock holds it for a pump at most: nothing waits
       inside it */
    ulong flags;
    lock_.lock(flags);
    pump_locked();
    arm_locked();
    lock_.unlock(flags);
}

bool Serial::wait()
{
    size_t pause_count = 1;

    /* Bounded: a stuck transmitter must not hang the kernel; give up
       and let the caller drop the character (~1M pauses total) */
    for (size_t round = 0; round < 20; round++)
    {
        if (is_transmit_empty())
            return true;

        for (size_t i = 0; i < pause_count; i++)
            cpu_relax();

        pause_count *= 2;
    }
    return is_transmit_empty();
}

bool Serial::is_transmit_empty()
{
    return (inb(PORT + REG_LSR) & LSR_TX_EMPTY) ? true : false;
}

Serial::~Serial()
{
    send();
}

void Serial::print_string(const char *str)
{
    if (!present_)
        return;

    ulong flags;
    lock_.lock(flags);
    for (;;)
    {
        char c = *str++;
        if (c == '\0')
        {
            break;
        }

        /* A full ring waits for room, the lock let go of: nothing dropped */
        if (c == '\n')
        {
            if (buf_.is_full())
            {
                lock_.unlock(flags);
                make_room();
                lock_.lock(flags);
            }
            buf_.put('\r');
        }

        if (buf_.is_full())
        {
            lock_.unlock(flags);
            make_room();
            lock_.lock(flags);
        }
        buf_.put(c);
    }
    pump_locked();
    if (tx_irq_)
        arm_locked();
    lock_.unlock(flags);

    /* Until the interrupt sends the rest, a write waits for its bytes */
    if (!tx_irq_)
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

    /* The first line of the report: what the ring has not sent yet came
       before it. Its newest PANIC_TAIL_BYTES go first, the rest -- in the
       dmesg, the netconsole and the disk log already -- said and skipped:
       the whole of a full ring is twenty seconds at 115200 baud, and the
       report's flush to the netconsole and the reset wait on this. No lock:
       the other CPUs are stopped, and one stopped holding it would hold it
       for good. Nothing more goes into the ring, nor out of it by the
       interrupt, which the IER write turns off. */
    if (!panic_started_)
    {
        panic_started_ = true;
        ier_ = 0;
        outb(PORT + REG_IER, ier_);

        size_t pending = buf_.get_size();
        if (pending > PANIC_TAIL_BYTES)
        {
            size_t skipped = pending - PANIC_TAIL_BYTES;
            for (size_t i = 0; i < skipped; i++)
                buf_.pop_head();

            char note[96];
            stdlib::sn_printf(note, sizeof(note), "\r\n[serial: %lu bytes before the panic not sent here]\r\n",
                static_cast<ulong>(skipped));
            for (const char* n = note; *n; n++)
            {
                wait();
                outb(PORT + REG_DATA, *n);
            }
        }
        while (!buf_.is_empty())
        {
            wait();
            outb(PORT + REG_DATA, buf_.get());
        }
    }

    /* Each character waited for as wait() waits, and no longer: a
       transmitter that never empties -- a UART that answered the probe and
       then went quiet -- kept this loop for good, and the screen, the
       netconsole and the disk log after it never had the report. Such a
       character is written anyway, into whatever the UART makes of it. */
    while (*str)
    {
        char c = *str++;
        if (c == '\n')
        {
            wait();
            outb(PORT, '\r');
        }
        wait();
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

    /* Received data's interrupt; the transmitter's comes on with
       start_tx_interrupt, once the vector is wired */
    ulong flags;
    lock_.lock(flags);
    ier_ = static_cast<u8>(ier_ | IER_RX_DATA);
    outb(PORT + REG_IER, ier_);
    lock_.unlock(flags);
}

InterruptHandlerFn Serial::get_handler_fn()
{
    return serial_interrupt_stub;
}

bool Serial::receive()
{
    for (ulong n = 0; n < RX_DRAIN_MAX; n++)
    {
        u8 lsr = inb(PORT + REG_LSR);

        /* Every bit of the status up at once is no UART's status: break,
           both errors, an overrun and an empty transmitter with data ready.
           It is what a port nothing decodes reads as -- a UART that
           answered the probe and is gone -- and a loop on its data-ready
           bit would never end, here with interrupts off. Its interrupts
           are turned off, should anything be there to hear it. */
        if (lsr == LSR_NOTHING_THERE)
        {
            outb(PORT + REG_IER, 0x00);
            return false;
        }
        if (!(lsr & LSR_DATA_READY))
            return true;

        u8 data = inb(PORT + REG_DATA);
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
    return true;
}

void Serial::interrupt(Context* ctx)
{
    (void)ctx;
    /* Its records (tp::Irq), the handler done as this returns */
    tp::Irq irq;
    if (unlikely(tp::Irq::on()))
        irq.begin((u32)int_vector_, tp::IrqKind::Serial);

    /* No UART, no handler's work: the port reads all ones, data ready
       among them, for ever */
    if (!present_)
    {
        hal::irq_eoi(int_vector_);
        return;
    }

    /* Until IIR says nothing is pending: the line is an ISA edge, and one
       left raised behind a cause not seen to raises no edge for the next */
    for (ulong round = 0; round < IRQ_ROUNDS_MAX; round++)
    {
        u8 iir = inb(PORT + REG_IIR);
        if (iir & IIR_NONE)
            break;

        u8 id = iir & IIR_ID_MASK;
        if (id == IIR_ID_TX_EMPTY)
        {
            /* IIR, read, has cleared it: the FIFO filled again, or the
               interrupt off with nothing left to send */
            ulong flags;
            lock_.lock(flags);
            if (!panic_started_)
            {
                pump_locked();
                arm_locked();
            }
            lock_.unlock(flags);
            continue;
        }
        if (id == IIR_ID_MODEM)
        {
            (void)inb(PORT + REG_MSR);
            continue;
        }
        if (id != IIR_ID_RX_DATA && id != IIR_ID_RX_TIMEOUT && id != IIR_ID_LINE)
            break;

        /* Received data, all the FIFO holds; a line status read away */
        if (!receive())
            break;
    }

    hal::irq_eoi(int_vector_);
}

extern "C" void serial_interrupt(Context* ctx)
{
    InterruptStats::inc(IrqSerial);
    Serial::get_instance().interrupt(ctx);
}

}