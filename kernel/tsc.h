#pragma once

#include <include/types.h>
#include <lib/stdlib.h>

namespace Kernel
{

struct PvClockVcpuTimeInfo
{
    u32 Version;
    u32 Pad0;
    u64 TscTimestamp;
    u64 SystemTime;
    u32 TscToSystemMul;
    s8  TscShift;
    u8  Flags;
    u8  Pad[2];
} __attribute__((packed));

static_assert(sizeof(PvClockVcpuTimeInfo) == 32, "Invalid size");

class Tsc
{
public:
    static Tsc& GetInstance()
    {
        static Tsc instance;
        return instance;
    }

    bool Calibrate();
    bool SetupKvmClock();

    ulong GetFreqHz();
    bool IsCalibrated();
    bool IsInvariant();
    bool IsKvmClockAvailable();

    /* Best-source time: kvmclock if available, else calibrated TSC */
    Stdlib::Time GetTime();

    /* Raw calibrated TSC time only (bypasses kvmclock) */
    Stdlib::Time TscTime();

    /* Raw TSC delta API */
    u64 ReadTscNow();
    ulong TscToNs(u64 deltaTsc);
    ulong ElapsedNs(u64 startTsc, u64 endTsc);

private:
    Tsc();
    ~Tsc();
    Tsc(const Tsc& other) = delete;
    Tsc(Tsc&& other) = delete;
    Tsc& operator=(const Tsc& other) = delete;
    Tsc& operator=(Tsc&& other) = delete;

    ulong CalibratePitCh2();
    Stdlib::Time KvmClockTime();

    /* PIT channel 2 calibration */
    static const u32 PitFrequency = 1193182;
    static const u16 PitChannel2Port = 0x42;
    static const u16 PitModePort = 0x43;
    static const u16 PitGatePort = 0x61;
    static const u8  PitCh2OneShotCmd = 0xB0; /* channel 2, lobyte/hibyte, mode 0 */
    static const u8  GateMask  = 0xFC; /* mask off gate + speaker bits */
    static const u8  GateOn    = 0x01; /* bit 0: gate enable */
    static const u8  GateOff   = 0xFE; /* ~GateOn */
    static const u8  OutPin    = 0x20; /* bit 5: channel 2 OUT pin */
    static const u32 CalibrationMs = 50;
    static const ulong CalibrationReload = (PitFrequency * CalibrationMs + 500) / 1000;
    static const ulong CalibrationRounds = 3;
    static const ulong MsPerSec = 1000;
    static const ulong HzPerMHz = 1000000;

    /* CPUID feature bits */
    static const u32 CpuidLeafFeatures    = 0x1;
    static const u32 CpuidLeafExtAdv      = 0x80000007;
    static const u32 CpuidLeafMaxExt      = 0x80000000;
    static const u32 CpuidBitTsc          = (1 << 4);  /* EDX of leaf 1 */
    static const u32 CpuidBitInvariantTsc = (1 << 8);  /* EDX of leaf 0x80000007 */

    /* KVM paravirt constants */
    static const u32 KvmCpuidSignature = 0x40000000;
    static const u32 KvmCpuidFeatures  = 0x40000001;
    static const u32 KvmFeatureClockSource2 = (1 << 3);
    static const u32 KvmMsrSystemTime = 0x4b564d01;
    static const u32 KvmMsrEnable     = 1;
    static const ulong KvmSignatureLen = 12;

    ulong FreqHz;
    u64 BaseTsc;
    bool Invariant;
    bool Calibrated;

    bool KvmClockAvail;
    PvClockVcpuTimeInfo* PvClock;
    ulong PvClockPhys;
};

}
