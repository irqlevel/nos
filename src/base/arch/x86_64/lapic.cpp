#include "lapic.h"
#include "percpu.h"
#include <kernel/time.h>
#include <drivers/acpi.h>

#include <kernel/trace.h>
#include "asm.h"
#include <mm/mmio.h>

namespace kernel
{

ulong Lapic::timer_ticks_per_sec_ = 0;

void* Lapic::get_reg_base(ulong index)
{
    return stdlib::mem_add(Acpi::get_instance().get_lapic_address(), index * 0x10);
}

u32 Lapic::read_reg(ulong index)
{
    hal::dma_rmb();
    return mm::MmIo::read32(get_reg_base(index));
}

void Lapic::write_reg(ulong index, u32 value)
{
    mm::MmIo::write32(get_reg_base(index), value);
    hal::dma_wmb();
}

void Lapic::enable()
{
    ulong msr = read_msr(BASE_MSR);

    trace(LAPIC_LL, "Lapic: msr 0x%lX base 0x%p", msr, Acpi::get_instance().get_lapic_address());

    /* Firmware may hand off with the APIC in x2APIC mode, where the
       MMIO window is dead. The SDM forbids switching x2APIC->xAPIC
       directly: go through the globally-disabled state first. */
    if (msr & BASE_MSR_X2_APIC_ENABLE)
    {
        trace(0, "Lapic: firmware left x2APIC mode, switching to xAPIC");
        write_msr(BASE_MSR, msr & ~(BASE_MSR_X2_APIC_ENABLE | BASE_MSR_GLOBAL_ENABLE));
        msr = msr & ~(BASE_MSR_X2_APIC_ENABLE | BASE_MSR_GLOBAL_ENABLE);
    }

    /* Make sure the APIC is globally enabled (xAPIC MMIO mode) */
    if (!(msr & BASE_MSR_GLOBAL_ENABLE))
    {
        msr = msr | BASE_MSR_GLOBAL_ENABLE;
        write_msr(BASE_MSR, msr);
    }

    write_reg(DFR_INDEX, 0xffffffff);// Flat mode
    write_reg(LDR_INDEX, 0x01000000);// All cpus use logical id 1
    write_reg(TPR_INDEX, 0xFF);// Disable all interrupts
    write_reg(SP_IV_INDEX, 0x100 | SPURIOUS_VECTOR);// bit 8 = APIC software enable

    trace(LAPIC_LL, "Lapic: tpr 0x%lX dfr 0x%lX ldr 0x%lX spiv 0x%lX",
        (ulong)read_reg(TPR_INDEX), (ulong)read_reg(DFR_INDEX), (ulong)read_reg(LDR_INDEX), (ulong)read_reg(SP_IV_INDEX));

    trace(LAPIC_LL, "Lapic: apic_id 0x%lX", (ulong)get_apic_id());

    /* The ID is known and the MMIO window is live: give this CPU its GS slot
       so nothing has to come back here to ask again. */
    per_cpu_publish(get_apic_id());

    mask_all_lvt();

    write_reg(EOI_INDEX, 0x0); // Acknowledge any outstanding interrupts

    write_reg(TPR_INDEX, 0x0);// Clear task priority to enable all interrupts
}

void Lapic::mask_lvt(ulong index, const char* name)
{
    u32 value = read_reg(index);

    if (value & LVT_MASKED)
        return;

    trace(0, "Lapic: masking lvt %s 0x%lX", name, (ulong)value);

    write_reg(index, value | LVT_MASKED);
}

void Lapic::mask_all_lvt()
{
    /* Firmware hands the APIC over configured for itself: virtual wire
       mode (LINT0 = ExtINT, so the 8259's INTR line reaches the CPU
       directly, and LINT1 = NMI) plus whatever timer, thermal or
       performance-counter entries it was using. nos routes every
       interrupt through the IOAPIC and masks the 8259 -- but a masked
       8259 still answers an ExtINT INTA cycle with its spurious IRQ7
       vector, and a leftover firmware LVT fires on a vector no driver
       owns. QEMU leaves the LVTs at their masked reset value, so an
       unmasked one only ever shows up on real machines.

       "Max LVT Entry" (version register bits 23:16) is the entry count
       minus one; reading the entries past it is undefined. */
    u32 max_lvt = (read_reg(VERSION_INDEX) >> 16) & 0xFF;

    mask_lvt(LVT_TIMER_INDEX, "timer");
    mask_lvt(LVT_LINT0_INDEX, "lint0");
    mask_lvt(LVT_LINT1_INDEX, "lint1");

    if (max_lvt >= 3)
        mask_lvt(LVT_ERROR_INDEX, "error");

    if (max_lvt >= 4)
        mask_lvt(LVT_PERF_CNT_INDEX, "perfcnt");

    if (max_lvt >= 5)
        mask_lvt(LVT_THERMAL_INDEX, "thermal");

    if (max_lvt >= 6)
        mask_lvt(LVT_CMCI_INDEX, "cmci");
}

bool Lapic::check_isr(u8 vector)
{
    ulong isr_reg_number = vector / 32;
    u8 reg_offset = vector % 32;

    return (read_reg(ISR_BASE_INDEX + isr_reg_number) & (1 << reg_offset)) ? true : false;
}

void Lapic::eoi()
{
    write_reg(EOI_INDEX, 0x0);
}

void Lapic::eoi(u8 vector)
{
    if (vector != SPURIOUS_VECTOR)
        write_reg(EOI_INDEX, 0x0);
}

u8 Lapic::get_apic_id()
{
    return read_reg(APIC_ID_INDEX) >> 24;
}

void Lapic::send_init(u32 apic_id)
{
    /* Same ICR interleaving hazard as send_ipi below */
    ulong flags = get_rflags();
    interrupt_disable();

    write_reg(ICR_HIGH_INDEX, apic_id << ICR_DESTINATION_SHIFT);
    write_reg(ICR_LOW_INDEX, ICR_INIT | ICR_PHYSICAL | ICR_ASSERT | ICR_EDGE | ICR_NO_SHORTHAND);

    while (read_reg(ICR_LOW_INDEX) & ICR_SEND_PENDING)
    {
        cpu_relax();
    }

    set_rflags(flags);
}

void Lapic::send_startup(u32 apic_id, u32 vector)
{
    /* Same ICR interleaving hazard as send_ipi below */
    ulong flags = get_rflags();
    interrupt_disable();

    write_reg(ICR_HIGH_INDEX, apic_id << ICR_DESTINATION_SHIFT);
    write_reg(ICR_LOW_INDEX, vector | ICR_STARTUP | ICR_PHYSICAL | ICR_ASSERT | ICR_EDGE | ICR_NO_SHORTHAND);

    while (read_reg(ICR_LOW_INDEX) & ICR_SEND_PENDING)
    {
        cpu_relax();
    }

    set_rflags(flags);
}

/* Delivery mode NMI: reaches a CPU with interrupts disabled, which an
   ordinary IPI cannot. No vector -- NMI is always vector 2. */
void Lapic::send_nmi(u32 apic_id)
{
    ulong flags = get_rflags();
    interrupt_disable();

    write_reg(ICR_HIGH_INDEX, apic_id << ICR_DESTINATION_SHIFT);
    write_reg(ICR_LOW_INDEX, ICR_NMI | ICR_PHYSICAL | ICR_ASSERT | ICR_EDGE | ICR_NO_SHORTHAND);

    while (read_reg(ICR_LOW_INDEX) & ICR_SEND_PENDING)
    {
        cpu_relax();
    }

    set_rflags(flags);
}

void Lapic::write_lvt_perf_counter_nmi()
{
    write_reg(LVT_PERF_CNT_INDEX, ICR_NMI);
}

void Lapic::mask_lvt_perf_counter()
{
    write_reg(LVT_PERF_CNT_INDEX, LVT_MASKED);
}

bool Lapic::calibrate_timer()
{
    static const ulong SAMPLE_NS = 50 * consts::NANO_SECS_IN_MS;

    /* Free-running and masked: nothing is delivered, the counter just
       counts down so we can see how fast it goes. */
    write_reg(TIMER_DIVIDE_INDEX, TIMER_DIVIDE_BY16);
    write_reg(LVT_TIMER_INDEX, LVT_MASKED);
    write_reg(TIMER_INITIAL_COUNT_INDEX, 0xFFFFFFFF);

    stdlib::Time start = get_boot_time();
    stdlib::Time deadline = start + stdlib::Time(SAMPLE_NS);
    while (get_boot_time() < deadline)
        cpu_relax();

    u32 remaining = read_reg(TIMER_CURRENT_COUNT_INDEX);
    ulong elapsed = (get_boot_time() - start).get_value();

    write_reg(TIMER_INITIAL_COUNT_INDEX, 0);

    if (remaining == 0 || elapsed == 0)
    {
        /* Ran the counter to zero, or the clock did not move: either way
           the measurement is not one to build a tick on. */
        trace(0, "Lapic: timer calibration failed (remaining %lu, elapsed %lu)",
            (ulong)remaining, elapsed);
        return false;
    }

    /* Under 2^32 counts in a sample of 50 ms: the product stays far below
       2^64 */
    ulong ticks = 0xFFFFFFFFUL - remaining;
    ulong ticks_per_sec = (ticks * consts::NANO_SECS_IN_SEC) / elapsed;

    /* Under 2^32 a second -- a bus of 68 GHz at the divide of 16 -- or the
       measurement is not of a local APIC; and arm_timer's arithmetic stays
       in a u64 below that. */
    if (ticks_per_sec == 0 || ticks_per_sec > 0xFFFFFFFFUL)
    {
        trace(0, "Lapic: timer rate %lu out of range", ticks_per_sec);
        return false;
    }

    timer_ticks_per_sec_ = ticks_per_sec;
    trace(0, "Lapic: timer %lu ticks/s", ticks_per_sec);

    return true;
}

void Lapic::start_timer(u8 vector)
{
    write_reg(TIMER_DIVIDE_INDEX, TIMER_DIVIDE_BY16);
    write_reg(LVT_TIMER_INDEX, (u32)vector | LVT_TIMER_ONE_SHOT);
    write_reg(TIMER_INITIAL_COUNT_INDEX, 0);
}

void Lapic::arm_timer(ulong delta_ns)
{
    ulong per_sec = timer_ticks_per_sec_;
    if (per_sec == 0)
        return;

    ulong late = delta_ns / TIMER_LATE_PARTS;
    ulong ns = (delta_ns > ~0UL - late) ? ~0UL : delta_ns + late;

    /* Whole seconds and the rest apart, the rest rounded up: a remainder
       under 2^30 times a rate under 2^32 (calibrate_timer) stays in a u64,
       and whole seconds past what the 32-bit count holds are its most */
    ulong secs = ns / consts::NANO_SECS_IN_SEC;
    ulong rest = ns % consts::NANO_SECS_IN_SEC;
    ulong ticks = MAX_TIMER_COUNT;
    if (secs <= MAX_TIMER_COUNT / per_sec)
    {
        ticks = secs * per_sec +
            (rest * per_sec + consts::NANO_SECS_IN_SEC - 1) / consts::NANO_SECS_IN_SEC;
    }

    /* A count of 0 stops the timer instead of firing it */
    ticks = stdlib::max(ticks, 1UL);
    ticks = stdlib::min(ticks, MAX_TIMER_COUNT);

    write_reg(TIMER_INITIAL_COUNT_INDEX, (u32)ticks);
}

void Lapic::send_ipi(u32 apic_id, u32 vector)
{
    /* The two ICR writes must not be interleaved with another send_ipi
       on this CPU (e.g. from an interrupt handler): the second sender
       would clobber IcrHigh and redirect the first IPI. */
    ulong flags = get_rflags();
    interrupt_disable();

    write_reg(ICR_HIGH_INDEX, apic_id << ICR_DESTINATION_SHIFT);
    write_reg(ICR_LOW_INDEX, vector | ICR_PHYSICAL | ICR_ASSERT | ICR_EDGE | ICR_NO_SHORTHAND);

    while (read_reg(ICR_LOW_INDEX) & ICR_SEND_PENDING)
    {
        cpu_relax();
    }

    set_rflags(flags);
}

}
