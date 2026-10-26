#include "pit.h"
#include <arch/x86_64/asm.h>
#include <hal/irqchip.h>

#include <kernel/cpu.h>
#include <kernel/interrupt.h>

namespace kernel
{

Pit::Pit()
    : int_vector_(-1)
    , time_ms_(0)
    , time_ms_ns_(0)
    , tick_ms_(0)
    , tick_ms_ns_(0)
    , reload_value_(0)
{
}

Pit::~Pit()
{
}

void Pit::setup()
{
    /*
     * Reload = 11932 → actual rate = 1193182 / 11932 = 99.99849... Hz
     * Period = 10.000150857... ms = 10 ms + 151 ns
     */
    reload_value_ = DESIRED_RELOAD;
    tick_ms_ = 1000 / DESIRED_HZ;
    /* Sub-ms nanosecond remainder per tick (rounded) */
    u64 period_ns = (u64)DESIRED_RELOAD * consts::NANO_SECS_IN_SEC;
    u64 whole_ms_ns = (u64)tick_ms_ * consts::NANO_SECS_IN_MS * BASE_FREQUENCY;
    tick_ms_ns_ = (ulong)((period_ns - whole_ms_ns + BASE_FREQUENCY / 2) / BASE_FREQUENCY);
    time_ms_ = 0;
    time_ms_ns_ = 0;

    outb(MODE_PORT, MODE_CH0_RATE_GEN);
    outb(CHANNEL0_PORT, stdlib::low_part(reload_value_));
    outb(CHANNEL0_PORT, stdlib::high_part(reload_value_));
}

void Pit::on_interrupt_register(u8 irq, u8 vector)
{
    (void)irq;
    int_vector_ = vector;
}

InterruptHandlerFn Pit::get_handler_fn()
{
    return pit_interrupt_stub;
}

void Pit::interrupt(Context* ctx)
{
    (void)ctx;
    {
        time_lock_.write_begin();

        time_ms_ = time_ms_ + tick_ms_;
        time_ms_ns_ = time_ms_ns_ + tick_ms_ns_;
        while (time_ms_ns_ >= consts::NANO_SECS_IN_MS)
        {
            time_ms_ns_ = time_ms_ns_ - consts::NANO_SECS_IN_MS;
            time_ms_ = time_ms_ + 1;
        }

        time_lock_.write_end();
    }

    CpuTable::get_instance().send_ipi_all();
    hal::irq_eoi(int_vector_);
}

stdlib::Time Pit::get_time()
{
    ulong ms, ms_ns;
    long seq;
    do {
        seq = time_lock_.read_begin();
        ms = time_ms_;
        ms_ns = time_ms_ns_;
    } while (time_lock_.read_retry(seq));

    return stdlib::Time(ms * consts::NANO_SECS_IN_MS + ms_ns);
}

extern "C" void pit_interrupt(Context* ctx)
{
    InterruptStats::inc(IrqPit);
    Pit::get_instance().interrupt(ctx);
}

void Pit::wait(const stdlib::Time& timeout)
{
    stdlib::Time expired = get_time() + timeout;

    while (get_time() < expired)
    {
        cpu_relax();
    }
}

void Pit::wait(ulong nano_secs)
{
    stdlib::Time timeout(nano_secs);

    wait(timeout);
}

}