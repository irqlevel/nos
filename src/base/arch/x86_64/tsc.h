#pragma once

#include <kernel/atomic.h>

#include <include/types.h>
#include <lib/stdlib.h>

namespace kernel
{

/* PVCLOCK_TSC_STABLE_BIT: the host keeps all vcpu entries in lockstep
   (masterclock), so readings are cross-CPU monotonic by construction. */
static const u8 PV_CLOCK_TSC_STABLE_BIT = 1;

struct PvClockVcpuTimeInfo
{
    u32 version;
    u32 pad0;
    u64 tsc_timestamp;
    u64 system_time;
    u32 tsc_to_system_mul;
    s8  tsc_shift;
    u8  flags;
    u8  pad[2];
} __attribute__((packed));

static_assert(sizeof(PvClockVcpuTimeInfo) == 32, "Invalid size");

class Tsc
{
public:
    static Tsc& get_instance()
    {
        static Tsc instance;
        return instance;
    }

    bool calibrate();
    bool setup_kvm_clock();

    /* Arm kvmclock for the calling CPU. KVM updates a pvclock entry only for
       the vcpu that wrote the MSR, so every CPU needs its own entry + MSR
       write. The BSP arms itself in setup_kvm_clock; APs call this from
       ap_main2. */
    bool enable_kvm_clock_self();

    ulong get_freq_hz();
    bool is_calibrated();
    bool is_invariant();
    bool is_kvm_clock_available();

    /* Best-source time: kvmclock if available, else calibrated TSC */
    stdlib::Time get_time();

    /* Raw calibrated TSC time only (bypasses kvmclock) */
    stdlib::Time tsc_time();

    /* Raw TSC delta API */
    u64 read_tsc_now();
    ulong tsc_to_ns(u64 delta_tsc);
    ulong elapsed_ns(u64 start_tsc, u64 end_tsc);

private:
    Tsc();
    ~Tsc();
    Tsc(const Tsc& other) = delete;
    Tsc(Tsc&& other) = delete;
    Tsc& operator=(const Tsc& other) = delete;
    Tsc& operator=(Tsc&& other) = delete;

    ulong calibrate_pit_ch2();
    ulong calibrate_hpet();
    stdlib::Time kvm_clock_time();

    /* PIT channel 2 calibration */
    static const u32 PIT_FREQUENCY = 1193182;
    static const u16 PIT_CHANNEL2_PORT = 0x42;
    static const u16 PIT_MODE_PORT = 0x43;
    static const u16 PIT_GATE_PORT = 0x61;
    static const u8  PIT_CH2_ONE_SHOT_CMD = 0xB0; /* channel 2, lobyte/hibyte, mode 0 */
    static const u8  GATE_MASK  = 0xFC; /* mask off gate + speaker bits */
    static const u8  GATE_ON    = 0x01; /* bit 0: gate enable */
    static const u8  GATE_OFF   = 0xFE; /* ~GATE_ON */
    static const u8  OUT_PIN    = 0x20; /* bit 5: channel 2 OUT pin */
    static const u32 CALIBRATION_MS = 50;
    static const ulong CALIBRATION_RELOAD = (PIT_FREQUENCY * CALIBRATION_MS + 500) / 1000;
    static const ulong CALIBRATION_ROUNDS = 3;
    static const ulong MS_PER_SEC = 1000;
    static const ulong HZ_PER_MHZ = 1000000;

    /* CPUID feature bits */
    static const u32 CPUID_LEAF_FEATURES    = 0x1;
    static const u32 CPUID_LEAF_EXT_ADV      = 0x80000007;
    static const u32 CPUID_LEAF_MAX_EXT      = 0x80000000;
    static const u32 CPUID_BIT_TSC          = (1 << 4);  /* EDX of leaf 1 */
    static const u32 CPUID_BIT_INVARIANT_TSC = (1 << 8);  /* EDX of leaf 0x80000007 */

    /* KVM paravirt constants */
    static const u32 KVM_CPUID_SIGNATURE = 0x40000000;
    static const u32 KVM_CPUID_FEATURES  = 0x40000001;
    static const u32 KVM_FEATURE_CLOCK_SOURCE2 = (1 << 3);
    static const u32 KVM_MSR_SYSTEM_TIME = 0x4b564d01;
    static const u32 KVM_MSR_ENABLE     = 1;
    static const ulong KVM_SIGNATURE_LEN = 12;

    ulong freq_hz_;
    u64 base_tsc_;
    bool invariant_;
    bool calibrated_;

    /* One 32-byte pvclock entry per APIC id in a single shared page */
    static const ulong PV_CLOCK_ENTRY_COUNT = 128;

    bool kvm_clock_avail_;
    PvClockVcpuTimeInfo* pv_clock_; /* page base: entry array indexed by APIC id */
    ulong pv_clock_phys_;

    /* Highest ns ever returned; enforces monotonicity when the host does
       not report PVCLOCK_TSC_STABLE_BIT (see kvm_clock_time). */
    Atomic last_ns_;
};

}
