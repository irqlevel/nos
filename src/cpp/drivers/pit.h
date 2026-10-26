#pragma once

#include <include/types.h>
#include <kernel/atomic.h>
#include <kernel/seq_lock.h>
#include <kernel/interrupt.h>
#include <hal/context.h>
#include <hal/irq_stubs.h>
#include <lib/stdlib.h>

namespace kernel
{

class Pit final : public InterruptHandler
{
public:
    static Pit& get_instance()
    {
        static Pit instance;

        return instance;
    }

    virtual void on_interrupt_register(u8 irq, u8 vector) override;
    virtual InterruptHandlerFn get_handler_fn() override;

    void interrupt(Context* ctx);

    void setup();

    stdlib::Time get_time();

    void wait(const stdlib::Time& timeout);
    void wait(ulong nano_secs);

private:
    Pit();
    ~Pit();

    Pit(const Pit& other) = delete;
    Pit(Pit&& other) = delete;
    Pit& operator=(const Pit& other) = delete;
    Pit& operator=(Pit&& other) = delete;

    int int_vector_;

    /* I/O ports */
    static const u16 CHANNEL0_PORT = 0x40;
    static const u16 CHANNEL2_PORT = 0x42;
    static const u16 MODE_PORT     = 0x43;

    /* Base oscillator frequency in Hz */
    static const u32 BASE_FREQUENCY = 1193182;

    /* Desired tick rate and derived reload value */
    static const u32 DESIRED_HZ = 100;
    static const u16 DESIRED_RELOAD = (BASE_FREQUENCY + DESIRED_HZ / 2) / DESIRED_HZ;

    /* Mode command: channel 0, lobyte/hibyte access, mode 2 (rate generator), binary */
    static const u8 MODE_CH0_RATE_GEN = 0x34;

    ulong time_ms_;
    ulong time_ms_ns_;
    ulong tick_ms_;
    ulong tick_ms_ns_;
    u16 reload_value_;

    SeqLock time_lock_;
};

}