#include "tsc.h"
#include "cpuid.h"
#include "asm.h"
#include "trace.h"

#include <include/const.h>
#include <mm/new.h>

namespace Kernel
{

Tsc::Tsc()
    : FreqHz(0)
    , BaseTsc(0)
    , Invariant(false)
    , Calibrated(false)
    , KvmClockAvail(false)
    , PvClock(nullptr)
    , PvClockPhys(0)
{
}

Tsc::~Tsc()
{
}

ulong Tsc::CalibratePitCh2()
{
    /* Program PIT channel 2 in one-shot mode */
    u8 gate = Inb(PitGatePort);
    gate = (gate & GateMask) | GateOn; /* gate on, speaker off */
    Outb(PitGatePort, gate);

    Outb(PitModePort, PitCh2OneShotCmd);

    /* Write reload value (lobyte then hibyte) */
    u16 reload = (u16)CalibrationReload;
    Outb(PitChannel2Port, (u8)(reload & 0xFF));
    Outb(PitChannel2Port, (u8)((reload >> 8) & 0xFF));

    /* Reset the gate to restart the countdown */
    gate = Inb(PitGatePort);
    Outb(PitGatePort, gate & GateOff);  /* gate low */
    Outb(PitGatePort, gate | GateOn);   /* gate high -- starts countdown */

    u64 tscStart = ReadTsc();

    /* Poll OUT pin until it goes high */
    while (!(Inb(PitGatePort) & OutPin))
        ;

    u64 tscEnd = ReadTsc();

    return (ulong)(tscEnd - tscStart);
}

bool Tsc::Calibrate()
{
    /* Check TSC presence */
    auto leaf1 = Cpuid(CpuidLeafFeatures);
    if (!(leaf1.Edx & CpuidBitTsc))
    {
        Trace(0, "TSC: not present");
        return false;
    }

    /* Check invariant TSC (only if extended CPUID supports this leaf) */
    auto maxExt = Cpuid(CpuidLeafMaxExt);
    if (maxExt.Eax >= CpuidLeafExtAdv)
    {
        auto leaf8007 = Cpuid(CpuidLeafExtAdv);
        Invariant = (leaf8007.Edx & CpuidBitInvariantTsc) != 0;
    }

    Trace(0, "TSC: invariant=%u", (ulong)Invariant);

    /* Calibrate using PIT channel 2 */
    ulong samples[CalibrationRounds];
    for (ulong i = 0; i < CalibrationRounds; i++)
        samples[i] = CalibratePitCh2();

    /* Sort and take median */
    for (ulong i = 0; i < CalibrationRounds - 1; i++)
    {
        for (ulong j = i + 1; j < CalibrationRounds; j++)
        {
            if (samples[j] < samples[i])
            {
                ulong tmp = samples[i];
                samples[i] = samples[j];
                samples[j] = tmp;
            }
        }
    }

    ulong medianTicks = samples[CalibrationRounds / 2];

    if (medianTicks == 0)
    {
        Trace(0, "TSC: calibration failed, zero ticks");
        return false;
    }

    /* FreqHz = medianTicks / (CalibrationMs / 1000)
     *        = medianTicks * 1000 / CalibrationMs */
    FreqHz = medianTicks * MsPerSec / CalibrationMs;
    BaseTsc = ReadTsc();
    Calibrated = true;

    Trace(0, "TSC: frequency %u Hz (%u MHz)", FreqHz, FreqHz / HzPerMHz);

    return true;
}

bool Tsc::SetupKvmClock()
{
    /* Check KVM signature: CPUID 0x40000000 -> "KVMKVMKVM\0\0\0" */
    auto sig = Cpuid(KvmCpuidSignature);
    u32 kvm[3] = { sig.Ebx, sig.Ecx, sig.Edx };
    const char* kvmStr = "KVMKVMKVM\0\0\0";
    if (Stdlib::MemCmp(kvm, kvmStr, KvmSignatureLen) != 0)
    {
        Trace(0, "TSC: not running under KVM");
        return false;
    }

    /* Check KVM_FEATURE_CLOCKSOURCE2 */
    auto feat = Cpuid(KvmCpuidFeatures);
    if (!(feat.Eax & KvmFeatureClockSource2))
    {
        Trace(0, "TSC: KVM detected but no clocksource2 feature");
        return false;
    }

    /* Allocate a page for the pvclock struct */
    PvClock = static_cast<PvClockVcpuTimeInfo*>(
        Mm::AllocMapPages(1, &PvClockPhys));
    if (!PvClock)
    {
        Trace(0, "TSC: failed to allocate pvclock page");
        return false;
    }

    Stdlib::MemSet(PvClock, 0, Const::PageSize);

    /* Enable kvmclock: write physical address | 1 (enable bit) to MSR */
    WriteMsr(KvmMsrSystemTime, PvClockPhys | KvmMsrEnable);

    KvmClockAvail = true;

    Trace(0, "TSC: kvmclock enabled, pvclock at phys 0x%p virt 0x%p",
        PvClockPhys, (ulong)PvClock);

    return true;
}

Stdlib::Time Tsc::KvmClockTime()
{
    u32 version;
    u64 ns;

    do {
        version = PvClock->Version;
        Barrier();

        u64 delta = ReadTsc() - PvClock->TscTimestamp;
        if (PvClock->TscShift >= 0)
            delta <<= PvClock->TscShift;
        else
            delta >>= (u8)(-PvClock->TscShift);

        ns = PvClock->SystemTime +
             (u64)(((__uint128_t)delta * PvClock->TscToSystemMul) >> 32);

        Barrier();
    } while ((PvClock->Version & 1) || PvClock->Version != version);

    return Stdlib::Time(ns);
}

Stdlib::Time Tsc::GetTime()
{
    if (KvmClockAvail)
        return KvmClockTime();

    if (Calibrated)
        return TscTime();

    return Stdlib::Time(0);
}

Stdlib::Time Tsc::TscTime()
{
    if (!Calibrated)
        return Stdlib::Time(0);

    u64 delta = ReadTsc() - BaseTsc;
    /* ns = delta * 1000000000 / FreqHz
     * To avoid overflow: split into seconds + remainder */
    u64 secs = delta / FreqHz;
    u64 rem = delta % FreqHz;
    u64 ns = secs * Const::NanoSecsInSec + rem * Const::NanoSecsInSec / FreqHz;
    return Stdlib::Time(ns);
}

u64 Tsc::ReadTscNow()
{
    return ReadTsc();
}

ulong Tsc::TscToNs(u64 deltaTsc)
{
    if (!Calibrated || FreqHz == 0)
        return 0;

    u64 secs = deltaTsc / FreqHz;
    u64 rem = deltaTsc % FreqHz;
    return (ulong)(secs * Const::NanoSecsInSec + rem * Const::NanoSecsInSec / FreqHz);
}

ulong Tsc::ElapsedNs(u64 startTsc, u64 endTsc)
{
    if (endTsc <= startTsc)
        return 0;

    return TscToNs(endTsc - startTsc);
}

ulong Tsc::GetFreqHz()
{
    return FreqHz;
}

bool Tsc::IsCalibrated()
{
    return Calibrated;
}

bool Tsc::IsInvariant()
{
    return Invariant;
}

bool Tsc::IsKvmClockAvailable()
{
    return KvmClockAvail;
}

}
