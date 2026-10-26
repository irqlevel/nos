#include "tsc.h"
#include "cpuid.h"
#include "asm.h"
#include <kernel/trace.h>
#include <kernel/preempt.h>

#include <include/const.h>
#include <mm/new.h>
#include <drivers/hpet.h>
#include <arch/x86_64/lapic.h>

namespace kernel
{

Tsc::Tsc()
    : freq_hz_(0)
    , base_tsc_(0)
    , invariant_(false)
    , calibrated_(false)
    , kvm_clock_avail_(false)
    , pv_clock_(nullptr)
    , pv_clock_phys_(0)
{
}

Tsc::~Tsc()
{
}

ulong Tsc::calibrate_pit_ch2()
{
    /* Program PIT channel 2 in one-shot mode */
    u8 gate = inb(PIT_GATE_PORT);
    gate = (gate & GATE_MASK) | GATE_ON; /* gate on, speaker off */
    outb(PIT_GATE_PORT, gate);

    outb(PIT_MODE_PORT, PIT_CH2_ONE_SHOT_CMD);

    /* Write reload value (lobyte then hibyte) */
    u16 reload = (u16)CALIBRATION_RELOAD;
    outb(PIT_CHANNEL2_PORT, (u8)(reload & 0xFF));
    outb(PIT_CHANNEL2_PORT, (u8)((reload >> 8) & 0xFF));

    /* Reset the gate to restart the countdown */
    gate = inb(PIT_GATE_PORT);
    outb(PIT_GATE_PORT, gate & GATE_OFF);  /* gate low */
    outb(PIT_GATE_PORT, gate | GATE_ON);   /* gate high -- starts countdown */

    u64 tsc_start = read_tsc();

    /* Poll OUT pin until it goes high */
    while (!(inb(PIT_GATE_PORT) & OUT_PIN))
        ;

    u64 tsc_end = read_tsc();

    return (ulong)(tsc_end - tsc_start);
}

ulong Tsc::calibrate_hpet()
{
    auto& hpet = Hpet::get_instance();
    if (!hpet.is_available())
        return 0;

    stdlib::Time t_start = hpet.get_time();
    stdlib::Time t_end = t_start + stdlib::Time(CALIBRATION_MS * consts::NANO_SECS_IN_MS);

    u64 tsc_start = read_tsc();
    while (hpet.get_time() < t_end)
    {
        cpu_relax();
    }
    u64 tsc_end = read_tsc();

    return (ulong)(tsc_end - tsc_start);
}

bool Tsc::calibrate()
{
    /* Check TSC presence */
    auto leaf1 = cpuid(CPUID_LEAF_FEATURES);
    if (!(leaf1.edx & CPUID_BIT_TSC))
    {
        trace(0, "TSC: not present");
        return false;
    }

    /* Check invariant TSC (only if extended CPUID supports this leaf) */
    auto max_ext = cpuid(CPUID_LEAF_MAX_EXT);
    if (max_ext.eax >= CPUID_LEAF_EXT_ADV)
    {
        auto leaf8007 = cpuid(CPUID_LEAF_EXT_ADV);
        invariant_ = (leaf8007.edx & CPUID_BIT_INVARIANT_TSC) != 0;
    }

    trace(0, "TSC: invariant=%lu", (ulong)invariant_);

    ulong samples[CALIBRATION_ROUNDS];

    /* Prefer HPET calibration over PIT channel 2 */
    if (Hpet::get_instance().is_available())
    {
        trace(0, "TSC: calibrating via HPET");
        for (ulong i = 0; i < CALIBRATION_ROUNDS; i++)
            samples[i] = calibrate_hpet();
    }
    else
    {
        /* Calibrate using PIT channel 2 */
        u8 saved_gate = inb(PIT_GATE_PORT);
        for (ulong i = 0; i < CALIBRATION_ROUNDS; i++)
            samples[i] = calibrate_pit_ch2();
        /* Restore port 0x61 to pre-calibration state (gate off, original bits) */
        outb(PIT_GATE_PORT, saved_gate & GATE_OFF);
    }

    /* Sort and take median */
    for (ulong i = 0; i < CALIBRATION_ROUNDS - 1; i++)
    {
        for (ulong j = i + 1; j < CALIBRATION_ROUNDS; j++)
        {
            if (samples[j] < samples[i])
            {
                ulong tmp = samples[i];
                samples[i] = samples[j];
                samples[j] = tmp;
            }
        }
    }

    ulong median_ticks = samples[CALIBRATION_ROUNDS / 2];

    if (median_ticks == 0)
    {
        trace(0, "TSC: calibration failed, zero ticks");
        return false;
    }

    /* freq_hz_ = median_ticks / (CALIBRATION_MS / 1000)
     *        = median_ticks * 1000 / CALIBRATION_MS */
    freq_hz_ = median_ticks * MS_PER_SEC / CALIBRATION_MS;
    base_tsc_ = read_tsc();
    calibrated_ = true;

    trace(0, "TSC: frequency %lu Hz (%lu MHz)", freq_hz_, freq_hz_ / HZ_PER_MHZ);

    return true;
}

bool Tsc::setup_kvm_clock()
{
    /* Check KVM signature: CPUID 0x40000000 -> "KVMKVMKVM\0\0\0" */
    auto sig = cpuid(KVM_CPUID_SIGNATURE);
    u32 kvm[3] = { sig.ebx, sig.ecx, sig.edx };
    const char* kvm_str = "KVMKVMKVM\0\0\0";
    if (stdlib::mem_cmp(kvm, kvm_str, KVM_SIGNATURE_LEN) != 0)
    {
        trace(0, "TSC: not running under KVM");
        return false;
    }

    /* Check KVM_FEATURE_CLOCKSOURCE2 */
    auto feat = cpuid(KVM_CPUID_FEATURES);
    if (!(feat.eax & KVM_FEATURE_CLOCK_SOURCE2))
    {
        trace(0, "TSC: KVM detected but no clocksource2 feature");
        return false;
    }

    /* Allocate a page of pvclock entries (one per APIC id) */
    pv_clock_ = static_cast<PvClockVcpuTimeInfo*>(
        mm::alloc_map_pages(1, &pv_clock_phys_));
    if (!pv_clock_)
    {
        trace(0, "TSC: failed to allocate pvclock page");
        return false;
    }

    stdlib::mem_set(pv_clock_, 0, consts::PAGE_SIZE);

    /* Enable kvmclock for the BSP; each AP arms its own entry in ap_main2 */
    if (!enable_kvm_clock_self())
        return false;

    kvm_clock_avail_ = true;

    trace(0, "TSC: kvmclock enabled, pvclock at phys 0x%lX virt 0x%lX",
        pv_clock_phys_, (ulong)pv_clock_);

    return true;
}

bool Tsc::enable_kvm_clock_self()
{
    if (pv_clock_ == nullptr)
        return false;

    ulong idx = Lapic::get_apic_id();
    if (idx >= PV_CLOCK_ENTRY_COUNT)
    {
        trace(0, "TSC: apic id %lu exceeds pvclock entries", idx);
        return false;
    }

    /* Write this entry's physical address | 1 (enable bit) to the MSR. KVM
       updates the entry only for the vcpu that wrote it. */
    write_msr(KVM_MSR_SYSTEM_TIME,
             (pv_clock_phys_ + idx * sizeof(PvClockVcpuTimeInfo)) | KVM_MSR_ENABLE);
    return true;
}

stdlib::Time Tsc::kvm_clock_time()
{
    u32 version;
    u64 ns;
    u8 flags;

    /* Read this CPU's own entry: an AP reading the BSP's entry would compute
       AP_tsc - BSP_timestamp, which is wrong unless TSCs are perfectly
       synchronized. A never-armed entry is all-zero (mul = 0) and yields 0,
       which get_boot_time treats as "fall back to HPET/PIT".

       The entry and the TSC must be sampled on the same vcpu: with
       interrupts enabled the task could be migrated between selecting the
       entry and executing rdtsc, mixing CPU A's parameters with CPU B's
       TSC. */
    ulong irq = preempt_irq_save();

    PvClockVcpuTimeInfo* pv = &pv_clock_[Lapic::get_apic_id() % PV_CLOCK_ENTRY_COUNT];

    do {
        version = pv->version;
        hal::smp_rmb();

        /* Same-vcpu rdtsc cannot precede the entry's timestamp; if the host
           republishes the entry mid-read the version loop retries, so only
           avoid the transient huge unsigned delta. */
        u64 now = read_tsc();
        u64 delta = (now >= pv->tsc_timestamp) ? now - pv->tsc_timestamp : 0;
        if (pv->tsc_shift >= 0)
            delta <<= pv->tsc_shift;
        else
            delta >>= (u8)(-pv->tsc_shift);

        ns = pv->system_time +
             (u64)(((__uint128_t)delta * pv->tsc_to_system_mul) >> 32);

        flags = pv->flags;
        hal::smp_rmb();
    } while ((pv->version & 1) || pv->version != version);

    preempt_irq_restore(irq);

    /* Without the stable bit (host left masterclock mode: live migration,
       host suspend, unstable host TSC) per-vcpu readings are not cross-CPU
       monotonic. Latch a global maximum so time never goes backwards --
       callers latch "now + timeout" deadlines, and a transient forward jump
       followed by correct readings would hang them forever. */
    if ((flags & PV_CLOCK_TSC_STABLE_BIT) == 0)
    {
        long seen = last_ns_.get();
        while ((long)ns > seen)
        {
            long old = last_ns_.cmpxchg((long)ns, seen);
            if (old == seen)
                return stdlib::Time(ns);
            seen = old;
        }
        return stdlib::Time((u64)seen);
    }

    return stdlib::Time(ns);
}

stdlib::Time Tsc::get_time()
{
    if (kvm_clock_avail_)
        return kvm_clock_time();

    if (calibrated_)
        return tsc_time();

    return stdlib::Time(0);
}

stdlib::Time Tsc::tsc_time()
{
    if (!calibrated_)
        return stdlib::Time(0);

    u64 delta = read_tsc() - base_tsc_;
    /* ns = delta * 1000000000 / freq_hz_
     * To avoid overflow: split into seconds + remainder */
    u64 secs = delta / freq_hz_;
    u64 rem = delta % freq_hz_;
    u64 ns = secs * consts::NANO_SECS_IN_SEC + rem * consts::NANO_SECS_IN_SEC / freq_hz_;
    return stdlib::Time(ns);
}

u64 Tsc::read_tsc_now()
{
    return read_tsc();
}

ulong Tsc::tsc_to_ns(u64 delta_tsc)
{
    if (!calibrated_ || freq_hz_ == 0)
        return 0;

    u64 secs = delta_tsc / freq_hz_;
    u64 rem = delta_tsc % freq_hz_;
    return (ulong)(secs * consts::NANO_SECS_IN_SEC + rem * consts::NANO_SECS_IN_SEC / freq_hz_);
}

ulong Tsc::elapsed_ns(u64 start_tsc, u64 end_tsc)
{
    if (end_tsc <= start_tsc)
        return 0;

    return tsc_to_ns(end_tsc - start_tsc);
}

ulong Tsc::get_freq_hz()
{
    return freq_hz_;
}

bool Tsc::is_calibrated()
{
    return calibrated_;
}

bool Tsc::is_invariant()
{
    return invariant_;
}

bool Tsc::is_kvm_clock_available()
{
    return kvm_clock_avail_;
}

}
