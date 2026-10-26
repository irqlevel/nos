#include "hpet.h"
#include "acpi.h"
#include <hal/irqchip.h>

#include <kernel/trace.h>
#include <kernel/cpu.h>
#include <kernel/interrupt.h>
#include <hal/cpu.h>
#include <hal/context.h>
#include <hal/irq_stubs.h>
#include <mm/page_table.h>
#include <include/const.h>

namespace kernel
{

Hpet::Hpet()
    : mmio_(nullptr)
    , period_fs_(0)
    , freq_hz_(0)
    , tick_period_ns_(0)
    , int_vector_(-1)
    , time_ns_(0)
{
}

Hpet::~Hpet()
{
}

u64 Hpet::read_reg(ulong offset) volatile
{
    return *reinterpret_cast<volatile u64*>(reinterpret_cast<ulong>(mmio_) + offset);
}

void Hpet::write_reg(ulong offset, u64 value) volatile
{
    *reinterpret_cast<volatile u64*>(reinterpret_cast<ulong>(mmio_) + offset) = value;
}

bool Hpet::setup()
{
    ulong base_phys = Acpi::get_instance().get_hpet_base_phys();
    if (base_phys == 0)
    {
        trace(0, "HPET: no HPET ACPI table, using PIT");
        return false;
    }

    /* Map one page of HPET MMIO */
    ulong va = mm::PageTable::get_instance().map_mmio_region(base_phys, consts::PAGE_SIZE);
    if (va == 0)
    {
        trace(0, "HPET: map_mmio_region failed for phys 0x%lX", base_phys);
        return false;
    }

    mmio_ = reinterpret_cast<volatile u64*>(va);

    /* Read capabilities: period in femtoseconds is in bits 63:32 */
    u64 gcap = read_reg(REG_GCAP_ID);
    period_fs_ = gcap >> GCAP_PERIOD_SHIFT;
    ulong num_timers = (ulong)((gcap & GCAP_NUM_TIM_MASK) >> GCAP_NUM_TIM_SHIFT) + 1;

    /* get_time assumes a full-width 64-bit main counter. A 32-bit counter
       (COUNT_SIZE_CAP == 0) wraps ~every 43s and breaks TSC calibration and
       the get_boot_time fallback. QEMU's HPET is 64-bit; warn on anything else. */
    if (!(gcap & GCAP_COUNT_SIZE_CAP))
        trace(0, "HPET: 32-bit main counter -- timekeeping wraps ~43s (unsupported hardware)");

    if (period_fs_ == 0 || period_fs_ > 0x05F5E100ULL /* 100 ns max per spec */)
    {
        trace(0, "HPET: invalid period %lu fs", (ulong)period_fs_);
        mmio_ = nullptr;
        return false;
    }

    /* Frequency = 1e15 / period_fs */
    freq_hz_ = (ulong)(1000000000000000ULL / period_fs_);

    /* Nanoseconds per tick at DESIRED_HZ */
    tick_period_ns_ = consts::NANO_SECS_IN_SEC / DESIRED_HZ;

    trace(0, "HPET: phys 0x%lX va 0x%lX period %lu fs freq %lu Hz timers %lu",
        base_phys, va, (ulong)period_fs_, freq_hz_, num_timers);

    /* Timer 0 must support periodic mode: the TYPE_CNF bit is ignored
       otherwise, the timer fires once and the system tick stops */
    if (!(read_reg(REG_TIMER0_CFG) & TIMER_PER_INT_CAP))
    {
        trace(0, "HPET: timer 0 has no periodic mode, using PIT");
        mmio_ = nullptr;
        return false;
    }

    /* Disable counter while we configure */
    u64 conf = read_reg(REG_GEN_CONF);
    conf &= ~CONF_ENABLE_CNF;
    write_reg(REG_GEN_CONF, conf);

    /* Reset main counter to 0 */
    write_reg(REG_MAIN_CNT, 0);

    /*
     * Configure timer 0 for periodic mode at DESIRED_HZ.
     * Steps per HPET spec:
     *   1. Set INT_ENB_CNF, TYPE_CNF (periodic), VAL_SET_CNF
     *   2. Write the period (in counter ticks) to the comparator twice:
     *      first write sets the initial compare value;
     *      second write (with VAL_SET_CNF still set) sets the accumulator.
     */
    u64 ticks_per_interrupt = freq_hz_ / DESIRED_HZ;
    u64 timer_cfg = TIMER_INT_ENB | TIMER_TYPE_PER | TIMER_VAL_SET;
    write_reg(REG_TIMER0_CFG, timer_cfg);
    write_reg(REG_TIMER0_CMP, ticks_per_interrupt); /* initial comparator */
    write_reg(REG_TIMER0_CMP, ticks_per_interrupt); /* periodic accumulator */

    /* Enable legacy replacement mode: timer 0 → IRQ 0 (replaces PIT),
       timer 1 → IRQ 8 (replaces RTC). This lets us register with the
       same GSI as the PIT, no explicit INT_ROUTE_CNF needed. */
    conf = read_reg(REG_GEN_CONF);
    conf |= CONF_ENABLE_CNF | CONF_LEG_RT_CNF;
    write_reg(REG_GEN_CONF, conf);

    trace(0, "HPET: configured, ticksPerInt %lu (100 Hz)", (ulong)ticks_per_interrupt);
    return true;
}

bool Hpet::is_available()
{
    return mmio_ != nullptr;
}

ulong Hpet::get_freq_hz()
{
    return freq_hz_;
}

stdlib::Time Hpet::get_time()
{
    if (mmio_ == nullptr)
        return stdlib::Time(0);

    u64 cnt = read_reg(REG_MAIN_CNT);
    /* ns = cnt * period_fs_ / 1e6; use 128-bit intermediate to avoid overflow */
    u64 ns = (u64)((__uint128_t)cnt * period_fs_ / FEMTO_SECS_PER_NANO_SEC);
    return stdlib::Time(ns);
}

void Hpet::wait(ulong nano_secs)
{
    stdlib::Time expired = get_time() + stdlib::Time(nano_secs);
    while (get_time() < expired)
    {
        cpu_relax();
    }
}

void Hpet::on_interrupt_register(u8 irq, u8 vector)
{
    (void)irq;
    int_vector_ = vector;
}

InterruptHandlerFn Hpet::get_handler_fn()
{
    return hpet_interrupt_stub;
}

void Hpet::interrupt(Context* ctx)
{
    (void)ctx;

    /* Clear the interrupt status for timer 0 */
    write_reg(REG_GEN_INT_STS, 1ULL);

    {
        time_lock_.write_begin();
        time_ns_ = time_ns_ + tick_period_ns_;
        time_lock_.write_end();
    }

    /* Timekeeping above is the job that stays here. The fan-out below is
       only the tick, and once every CPU runs its own local timer there is
       nobody left to wake. */
    if (!CpuTable::get_instance().has_per_cpu_timer())
        CpuTable::get_instance().send_ipi_all();

    hal::irq_eoi(int_vector_);
}

extern "C" void hpet_interrupt(Context* ctx)
{
    InterruptStats::inc(IrqHpet);
    Hpet::get_instance().interrupt(ctx);
}

}
