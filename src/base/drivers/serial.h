#pragma once

#include <include/types.h>
#include <kernel/spin_lock.h>
#include <kernel/interrupt.h>
#include <kernel/timer.h>
#include <hal/context.h>
#include <lib/ring_buffer.h>

namespace kernel
{

class SerialObserver
{
public:
    virtual void on_char(char c, u8 code) = 0;
};

/* The 16550 at COM1. What is written goes into a ring and out of it into
   the UART's FIFO. Until its interrupt is set up a write waits for all of
   it to go out, as it always did; after that, a write takes what the FIFO
   can take at once and the interrupt for "FIFO empty" sends the rest. At
   115200 baud a line of the log is eight milliseconds, and every line
   waited out cost the CPU that logged it those eight -- nine of Newton's
   sixteen seconds of boot, its COM1 not even wired to anything. A ring that
   fills waits as before: nothing is dropped. */
class Serial final : public InterruptHandler, public TimerCallback
{
public:
    static Serial& get_instance()
    {
        static Serial instance;

        return instance;
    }

    void print_string(const char *str);
    void panic_print_string(const char *str);

    PRINTF_FORMAT(2, 0) void v_printf(const char *fmt, va_list args);
    PRINTF_FORMAT(2, 3) void printf(const char *fmt, ...);

    void backspace();

    virtual void on_interrupt_register(u8 irq, u8 vector) override;
    virtual InterruptHandlerFn get_handler_fn() override;

    void interrupt(Context* ctx);

    /* Everything written so far sent, waited for: before a halt or a reset,
       where the next instant may be the machine's last */
    void flush();

    /* Once the interrupt is registered: writes from now on return once
       their bytes are in the ring, and a timer sends on what an interrupt
       that never came left there */
    void start_tx_interrupt();
    /* Going down -- a halt, a reset: the interrupt off and the ring sent
       now, its newest bytes, the rest said and skipped; every write after
       this waits for its bytes again */
    void stop_tx_interrupt();

    /* The timer's look, ten times a second: bytes waiting, an empty FIFO
       and no interrupt to send them is an interrupt lost */
    virtual void on_tick(TimerCallback& callback) override;

    /* False when nothing decodes the UART port -- the machine has no
       serial console and the screen is the only diagnostic channel. */
    bool is_present();

    bool register_observer(SerialObserver& observer);
    void unregister_observer(SerialObserver& observer);

private:
    Serial();
    ~Serial();

    /* What the ring holds into the FIFO while the transmitter says it is
       empty, as much as the FIFO holds a time: no wait. The lock held. */
    void pump_locked();
    /* The "transmitter empty" interrupt on while the ring has bytes, off
       once it has none. The lock held. */
    void arm_locked();
    /* The ring emptied into the UART, each FIFO's worth waited for */
    void send();
    /* What the receiver holds, to the observers; false for a UART gone */
    bool receive();
    /* Room in a full ring: a FIFO's worth sent, waited for */
    void make_room();
    /* Whether the transmitter came empty within a bound */
    bool wait();

    Serial(const Serial& other) = delete;
    Serial(Serial&& other) = delete;
    Serial& operator=(const Serial& other) = delete;
    Serial& operator=(Serial&& other) = delete;

    bool is_transmit_empty();

    bool present_;
    int int_vector_;
    /* How many bytes the transmitter takes at once once it says empty: a
       16550A's FIFO, 16; a UART without one, 1 */
    size_t fifo_depth_;
    /* Set by start_tx_interrupt: a write no longer waits for its bytes */
    volatile bool tx_irq_;
    /* What IER holds, under the lock */
    u8 ier_;
    /* The panic's first line has dealt with the ring: what of it to send
       before the report, what to skip */
    bool panic_started_;

    /* The boot's log fits -- 150 KiB of it on Newton, written in seconds
       and sent in thirteen */
    static const size_t TX_RING_BYTES = 256 * 1024;
    stdlib::RingBuffer<char, TX_RING_BYTES> buf_;
    SpinLock lock_;

    static const int PORT = 0x3F8;

    static const size_t MAX_OBSERVER = 4;
    SerialObserver* rx_observer_[MAX_OBSERVER];
};

}
