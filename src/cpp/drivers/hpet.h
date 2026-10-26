#pragma once

#include <include/types.h>
#include <kernel/seq_lock.h>
#include <kernel/interrupt.h>
#include <lib/stdlib.h>

namespace kernel
{

/*
 * HPET (High Precision Event Timer) driver.
 *
 * When available (ACPI HPET table present), this driver replaces the PIT
 * as the system tick source and provides TSC calibration.
 *
 * Comparator 0 is programmed for 100 Hz periodic mode; the interrupt
 * broadcasts an IPI to all CPUs exactly like PIT::Interrupt.
 *
 * GetTime() reads the MAIN_CNT register directly for sub-microsecond
 * precision without depending on the interrupt counter.
 *
 * Legacy replacement mode (LEG_RT_CNF) is used: timer 0 replaces
 * PIT on IRQ 0 and timer 1 replaces RTC on IRQ 8.  This avoids the
 * need for explicit INT_ROUTE_CNF configuration and lets us register
 * with the same GSI as PIT.
 */
class Hpet final : public InterruptHandler
{
public:
    /* Tick rate, shared with whatever drives the tick: the HPET's own
       comparator, or each CPU's local APIC timer. */
    static const ulong DESIRED_HZ = 100;

    static Hpet& get_instance()
    {
        static Hpet instance;
        return instance;
    }

    /* Map MMIO from ACPI, read GCAP, configure comparator 0.
       Must be called after Acpi::Parse() and after page allocator is up.
       Returns false if no HPET ACPI table or mapping fails. */
    bool setup();

    /* true once Setup() succeeds */
    bool is_available();

    /* Monotonic time read directly from MAIN_CNT (high precision) */
    stdlib::Time get_time();

    /* Busy-spin wait using HPET counter */
    void wait(ulong nano_secs);

    /* Frequency of the HPET main counter in Hz */
    ulong get_freq_hz();

    /* InterruptHandler interface */
    virtual void on_interrupt_register(u8 irq, u8 vector) override;
    virtual InterruptHandlerFn get_handler_fn() override;

    void interrupt(Context* ctx);

private:
    Hpet();
    ~Hpet();
    Hpet(const Hpet& other) = delete;
    Hpet(Hpet&& other) = delete;
    Hpet& operator=(const Hpet& other) = delete;
    Hpet& operator=(Hpet&& other) = delete;

    /* HPET MMIO register offsets */
    static const ulong REG_GCAP_ID    = 0x000; /* capabilities + period (64-bit) */
    static const ulong REG_GEN_CONF   = 0x010; /* general configuration (64-bit) */
    static const ulong REG_GEN_INT_STS = 0x020; /* general interrupt status (64-bit) */
    static const ulong REG_MAIN_CNT   = 0x0F0; /* main counter (64-bit) */
    static const ulong REG_TIMER0_CFG = 0x100; /* timer 0 config/caps (64-bit) */
    static const ulong REG_TIMER0_CMP = 0x108; /* timer 0 comparator (64-bit) */

    /* GEN_CONF bits */
    static const u64 CONF_ENABLE_CNF  = (1ULL << 0); /* start/stop main counter */
    static const u64 CONF_LEG_RT_CNF   = (1ULL << 1); /* legacy replacement mode */

    /* Timer config bits */
    static const u64 TIMER_INT_ENB    = (1ULL << 2); /* enable interrupt */
    static const u64 TIMER_TYPE_PER   = (1ULL << 3); /* periodic mode */
    static const u64 TIMER_PER_INT_CAP = (1ULL << 4); /* periodic mode supported (RO) */
    static const u64 TIMER_VAL_SET    = (1ULL << 6); /* set accumulator in periodic mode */
    static const u64 TIMER32_MODE    = (1ULL << 8); /* force 32-bit comparator */

    /* GCAP_ID field extraction */
    static const u64 GCAP_PERIOD_SHIFT = 32; /* counter period in femtoseconds */
    static const u64 GCAP_NUM_TIM_MASK  = 0x1F00; /* bits 12:8 */
    static const u64 GCAP_NUM_TIM_SHIFT = 8;
    static const u64 GCAP_COUNT_SIZE_CAP = (1ULL << 13); /* 1 = 64-bit main counter */

    /* Desired tick rate (same as PIT) */


    /* 1 femtosecond = 1e-15 s; GCAP period is in femtoseconds */
    static const u64 FEMTO_SECS_PER_NANO_SEC = 1000000ULL;

    volatile u64* mmio_;  /* mapped HPET MMIO base (null = not available) */
    u64 period_fs_;        /* counter period in femtoseconds */
    ulong freq_hz_;       /* derived counter frequency */
    ulong tick_period_ns_;  /* nanoseconds per tick at DesiredHz */
    int int_vector_;

    /* Interrupt-driven coarse counter (kept in sync for callers that use it) */
    ulong time_ns_;
    SeqLock time_lock_;

    u64 read_reg(ulong offset) volatile;
    void write_reg(ulong offset, u64 value) volatile;
};

}
