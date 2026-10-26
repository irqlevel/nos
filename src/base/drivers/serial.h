#pragma once

#include <include/types.h>
#include <kernel/spin_lock.h>
#include <kernel/interrupt.h>
#include <hal/context.h>
#include <lib/ring_buffer.h>

namespace kernel
{

class SerialObserver
{
public:
    virtual void on_char(char c, u8 code) = 0;
};

class Serial final : public InterruptHandler
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

    void flush();

    /* False when nothing decodes the UART port -- the machine has no
       serial console and the screen is the only diagnostic channel. */
    bool is_present();

    bool register_observer(SerialObserver& observer);
    void unregister_observer(SerialObserver& observer);

private:
    Serial();
    ~Serial();

    void write_char(char c);
    void send();
    void wait();

    Serial(const Serial& other) = delete;
    Serial(Serial&& other) = delete;
    Serial& operator=(const Serial& other) = delete;
    Serial& operator=(Serial&& other) = delete;

    bool is_transmit_empty();

    bool present_;
    int int_vector_;

    stdlib::RingBuffer<char, 4 * consts::PAGE_SIZE> buf_;
    SpinLock lock_;

    static const int PORT = 0x3F8;

    static const size_t MAX_OBSERVER = 4;
    SerialObserver* rx_observer_[MAX_OBSERVER];
};

}