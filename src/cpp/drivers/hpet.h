#pragma once

#include <include/types.h>
#include <kernel/seq_lock.h>
#include <kernel/interrupt.h>
#include <lib/stdlib.h>

namespace Kernel
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
    static Hpet& GetInstance()
    {
        static Hpet Instance;
        return Instance;
    }

    /* Map MMIO from ACPI, read GCAP, configure comparator 0.
       Must be called after Acpi::Parse() and after page allocator is up.
       Returns false if no HPET ACPI table or mapping fails. */
    bool Setup();

    /* true once Setup() succeeds */
    bool IsAvailable();

    /* Monotonic time read directly from MAIN_CNT (high precision) */
    Stdlib::Time GetTime();

    /* Busy-spin wait using HPET counter */
    void Wait(ulong nanoSecs);

    /* Frequency of the HPET main counter in Hz */
    ulong GetFreqHz();

    /* InterruptHandler interface */
    virtual void OnInterruptRegister(u8 irq, u8 vector) override;
    virtual InterruptHandlerFn GetHandlerFn() override;

    void Interrupt(Context* ctx);

private:
    Hpet();
    ~Hpet();
    Hpet(const Hpet& other) = delete;
    Hpet(Hpet&& other) = delete;
    Hpet& operator=(const Hpet& other) = delete;
    Hpet& operator=(Hpet&& other) = delete;

    /* HPET MMIO register offsets */
    static const ulong RegGcapId    = 0x000; /* capabilities + period (64-bit) */
    static const ulong RegGenConf   = 0x010; /* general configuration (64-bit) */
    static const ulong RegGenIntSts = 0x020; /* general interrupt status (64-bit) */
    static const ulong RegMainCnt   = 0x0F0; /* main counter (64-bit) */
    static const ulong RegTimer0Cfg = 0x100; /* timer 0 config/caps (64-bit) */
    static const ulong RegTimer0Cmp = 0x108; /* timer 0 comparator (64-bit) */

    /* GEN_CONF bits */
    static const u64 ConfEnableCnf  = (1ULL << 0); /* start/stop main counter */
    static const u64 ConfLegRtCnf   = (1ULL << 1); /* legacy replacement mode */

    /* Timer config bits */
    static const u64 TimerIntEnb    = (1ULL << 2); /* enable interrupt */
    static const u64 TimerTypePer   = (1ULL << 3); /* periodic mode */
    static const u64 TimerValSet    = (1ULL << 6); /* set accumulator in periodic mode */
    static const u64 Timer32Mode    = (1ULL << 8); /* force 32-bit comparator */

    /* GCAP_ID field extraction */
    static const u64 GcapPeriodShift = 32; /* counter period in femtoseconds */
    static const u64 GcapNumTimMask  = 0x1F00; /* bits 12:8 */
    static const u64 GcapNumTimShift = 8;

    /* Desired tick rate (same as PIT) */
    static const ulong DesiredHz = 100;

    /* 1 femtosecond = 1e-15 s; GCAP period is in femtoseconds */
    static const u64 FemtoSecsPerNanoSec = 1000000ULL;

    volatile u64* Mmio;  /* mapped HPET MMIO base (null = not available) */
    u64 PeriodFs;        /* counter period in femtoseconds */
    ulong FreqHz_;       /* derived counter frequency */
    ulong TickPeriodNs;  /* nanoseconds per tick at DesiredHz */
    int IntVector;

    /* Interrupt-driven coarse counter (kept in sync for callers that use it) */
    ulong TimeNs;
    SeqLock TimeLock;

    u64 ReadReg(ulong offset) volatile;
    void WriteReg(ulong offset, u64 value) volatile;
};

}
