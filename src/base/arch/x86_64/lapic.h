#pragma once

#include <kernel/spin_lock.h>
#include <lib/stdlib.h>

namespace kernel
{

class Lapic final
{
public:

    /* LAPIC spurious-interrupt vector. The APIC delivers this vector on a
       spurious interrupt; its handler must NOT issue an EOI. */
    static const u8 SPURIOUS_VECTOR = 0xFF;

    static void enable();

    static void eoi();

    static void eoi(u8 vector);

    static u8 get_apic_id();

    static void send_init(u32 apic_id);

    static void send_startup(u32 apic_id, u32 vector);

    static void send_ipi(u32 apic_id, u32 vector);

    static void send_nmi(u32 apic_id);

    /* The local APIC's performance-counter LVT entry, in NMI delivery mode.
       Rewritten after every delivery: the hardware masks the entry as it
       delivers. */
    static void write_lvt_perf_counter_nmi();
    static void mask_lvt_perf_counter();

    /* Counts the local timer against the established clock: its rate,
       kept for arm_timer, and false if it could not be measured. Runs once,
       on the BSP: the rate is a property of the bus clock, which every core
       shares. */
    static bool calibrate_timer();

    /* This CPU's local timer, one-shot on the given vector, not armed. */
    static void start_timer(u8 vector);

    /* Fire this CPU's local timer once, delta_ns from now: a write of the
       initial count, which restarts it (hal::local_timer_arm). */
    static void arm_timer(ulong delta_ns);

    /* Is `vector` currently in service on this CPU? Used by the shared
       interrupt dispatch to identify the vector that actually fired. */
    static bool check_isr(u8 vector);

private:
    Lapic() = delete;
    ~Lapic() = delete;
    Lapic(const Lapic& other) = delete;
    Lapic(Lapic&& other) = delete;
    Lapic& operator=(const Lapic& other) = delete;
    Lapic& operator=(Lapic&& other) = delete;

    static u32 read_reg(ulong index);
    static void write_reg(ulong index, u32 value);
    static void* get_reg_base(ulong index);

    static void mask_lvt(ulong index, const char* name);
    static void mask_all_lvt();

    static const ulong APIC_ID_INDEX = 2;
    static const ulong VERSION_INDEX = 0x3;
    static const ulong TPR_INDEX = 0x8;
    static const ulong LDR_INDEX = 0xD;
    static const ulong DFR_INDEX = 0xE;
    static const ulong EOI_INDEX = 0xB;
    static const ulong SP_IV_INDEX = 0xF;
    static const ulong ISR_BASE_INDEX = 0x10;

    /* Local vector table (SDM vol.3 "Local Vector Table"); CMCI and the
       entries past the version register's Max LVT Entry field are absent
       on older CPUs */
    static const ulong LVT_CMCI_INDEX = 0x2F;
    static const ulong LVT_TIMER_INDEX = 0x32;
    static const ulong LVT_THERMAL_INDEX = 0x33;
    static const ulong LVT_PERF_CNT_INDEX = 0x34;
    static const ulong LVT_LINT0_INDEX = 0x35;
    static const ulong LVT_LINT1_INDEX = 0x36;
    static const ulong LVT_ERROR_INDEX = 0x37;

    static const u32 LVT_MASKED = (1U << 16);

    static const ulong TIMER_INITIAL_COUNT_INDEX = 0x38;
    static const ulong TIMER_CURRENT_COUNT_INDEX = 0x39;
    static const ulong TIMER_DIVIDE_INDEX = 0x3E;

    /* Divide-configuration encoding for /16 (SDM 11.5.4). */
    static const u32 TIMER_DIVIDE_BY16 = 0x3;

    /* The timer mode, bits 18:17: one-shot is zero. */
    static const u32 LVT_TIMER_ONE_SHOT = 0;

    /* Counts of the local timer a second, at the divide above: measured
       once by calibrate_timer, 0 until then. */
    static ulong timer_ticks_per_sec_;

    /* arm_timer counts a part in this many over the delay it is asked for:
       the rate is a measurement, and one a little fast would fire before a
       sleeper's deadline -- for one more interrupt to take it. Late by as
       much costs nothing. */
    static const ulong TIMER_LATE_PARTS = 1024;

    /* The initial-count register's 32 bits. */
    static constexpr ulong MAX_TIMER_COUNT = 0xFFFFFFFFUL;

    static const ulong ICR_LOW_INDEX = 0x30;
    static const ulong ICR_HIGH_INDEX = 0x31;

    static const u32 ICR_FIXED = 0x0;
    static const u32 ICR_LOWEST = 0x100;
    static const u32 ICR_SMI = 0x200;
    static const u32 ICR_NMI = 0x400;
    static const u32 ICR_INIT = 0x500;
    static const u32 ICR_STARTUP = 0x600;
    static const u32 ICR_PHYSICAL = 0x0;
    static const u32 ICR_LOGICAL = 0x800;
    static const u32 ICR_IDLE = 0x0;
    static const u32 ICR_SEND_PENDING = 0x1000;
    static const u32 ICR_DEASSERT = 0x0;
    static const u32 ICR_ASSERT = 0x4000;
    static const u32 ICR_EDGE = 0x0;
    static const u32 ICR_LEVEL = 0x8000;
    static const u32 ICR_NO_SHORTHAND = 0x0;
    static const u32 ICR_SELF = 0x40000;
    static const u32 ICR_ALL_INCLUDING_SELF = 0x80000;
    static const u32 ICR_ALL_EXCLUDING_SELF = 0xc0000;
    static const u32 ICR_DESTINATION_SHIFT = 24;

    static const ulong BASE_MSR = 0x1B;
    static const ulong BASE_MSR_GLOBAL_ENABLE = (1UL << 11);
    static const ulong BASE_MSR_X2_APIC_ENABLE = (1UL << 10);

};

}