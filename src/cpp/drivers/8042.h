#pragma once

#include <include/types.h>
#include <kernel/timer.h>
#include <kernel/spin_lock.h>
#include <kernel/interrupt.h>
#include <hal/context.h>
#include <kernel/atomic.h>
#include <kernel/input.h>
#include <lib/ring_buffer.h>

namespace kernel
{

class IO8042 : public TimerCallback, public InterruptHandler
{
public:
    static IO8042& get_instance()
    {
        static IO8042 instance;
        return instance;
    }

    virtual void on_interrupt_register(u8 irq, u8 vector) override;
    virtual InterruptHandlerFn get_handler_fn() override;

    void interrupt(Context* ctx);

    virtual void on_tick(TimerCallback& callback) override;

    char get_cmd();

private:
    IO8042();
    virtual ~IO8042();
    void read_data();

    IO8042(const IO8042& other) = delete;
    IO8042(IO8042&& other) = delete;
    IO8042& operator=(const IO8042& other) = delete;
    IO8042& operator=(IO8042&& other) = delete;

    static const ulong DATA_PORT = 0x60;
    static const ulong STATUS_PORT = 0x64;

    /* Bound on the ReadData drain loop (stuck status bit) */
    static const size_t MAX_DRAIN_ITERATIONS = 4096;

    /* Set-1 scan codes: a key's make code, its break code the same with
       CodeBreak set; behind CodeExtended, the right Ctrl and the arrows */
    static const u8 CODE_BREAK = 0x80;
    static const u8 CODE_EXTENDED = 0xE0;
    static const u8 CODE_CTRL = 0x1D;
    static const u8 CODE_LEFT_SHIFT = 0x2A;
    static const u8 CODE_RIGHT_SHIFT = 0x36;
    static const u8 CODE_UP = 0x48;
    static const u8 CODE_LEFT = 0x4B;
    static const u8 CODE_RIGHT = 0x4D;
    static const u8 CODE_DOWN = 0x50;

    /* Ctrl keeps a letter's low five bits: 'r' (0x72) is ^R (0x12) */
    static const u8 CTRL_MASK = 0x1F;

    /* What a terminal sends for an arrow, which the shell's editor reads:
       nullptr for any other extended key */
    static const char* arrow_sequence(u8 code);

    SpinLock lock_;
    stdlib::RingBuffer<u8, consts::PAGE_SIZE> buf_;

    int int_vector_;
    u8 mod_;
    u8 ctrl_;
    u8 extended_; /* set after a 0xE0 prefix byte */

    Atomic interrupt_counter_;
};

}
