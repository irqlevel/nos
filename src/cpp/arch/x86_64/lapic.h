#pragma once

#include <kernel/spin_lock.h>
#include <lib/stdlib.h>

namespace Kernel
{

class Lapic final
{
public:

    /* LAPIC spurious-interrupt vector. The APIC delivers this vector on a
       spurious interrupt; its handler must NOT issue an EOI. */
    static const u8 SpuriousVector = 0xFF;

    static void Enable();

    static void EOI();

    static void EOI(u8 vector);

    static u8 GetApicId();

    static void SendInit(u32 apicId);

    static void SendStartup(u32 apicId, u32 vector);

    static void SendIPI(u32 apicId, u32 vector);

    static void SendNmi(u32 apicId);

    /* The local APIC's performance-counter LVT entry, in NMI delivery mode.
       Rewritten after every delivery: the hardware masks the entry as it
       delivers. */
    static void WriteLvtPerfCounterNmi();
    static void MaskLvtPerfCounter();

    /* Counts the local timer against the established clock and returns the
       initial-count value for hz ticks a second, or 0 if it could not be
       measured. Runs once, on the BSP: the count is a property of the bus
       clock, which every core shares. */
    static u32 CalibrateTimer(ulong hz);

    /* Arm this CPU's local timer, periodic, on the given vector. */
    static void StartTimer(u8 vector, u32 initialCount);

    /* Is `vector` currently in service on this CPU? Used by the shared
       interrupt dispatch to identify the vector that actually fired. */
    static bool CheckIsr(u8 vector);

private:
    Lapic() = delete;
    ~Lapic() = delete;
    Lapic(const Lapic& other) = delete;
    Lapic(Lapic&& other) = delete;
    Lapic& operator=(const Lapic& other) = delete;
    Lapic& operator=(Lapic&& other) = delete;

    static u32 ReadReg(ulong index);
    static void WriteReg(ulong index, u32 value);
    static void* GetRegBase(ulong index);

    static void MaskLvt(ulong index, const char* name);
    static void MaskAllLvt();

    static const ulong ApicIdIndex = 2;
    static const ulong VersionIndex = 0x3;
    static const ulong TprIndex = 0x8;
    static const ulong LdrIndex = 0xD;
    static const ulong DfrIndex = 0xE;
    static const ulong EoiIndex = 0xB;
    static const ulong SpIvIndex = 0xF;
    static const ulong IsrBaseIndex = 0x10;

    /* Local vector table (SDM vol.3 "Local Vector Table"); CMCI and the
       entries past the version register's Max LVT Entry field are absent
       on older CPUs */
    static const ulong LvtCmciIndex = 0x2F;
    static const ulong LvtTimerIndex = 0x32;
    static const ulong LvtThermalIndex = 0x33;
    static const ulong LvtPerfCntIndex = 0x34;
    static const ulong LvtLint0Index = 0x35;
    static const ulong LvtLint1Index = 0x36;
    static const ulong LvtErrorIndex = 0x37;

    static const u32 LvtMasked = (1U << 16);

    static const ulong TimerInitialCountIndex = 0x38;
    static const ulong TimerCurrentCountIndex = 0x39;
    static const ulong TimerDivideIndex = 0x3E;

    /* Divide-configuration encoding for /16 (SDM 11.5.4). */
    static const u32 TimerDivideBy16 = 0x3;

    static const u32 LvtTimerPeriodic = (1u << 17);

    static const ulong IcrLowIndex = 0x30;
    static const ulong IcrHighIndex = 0x31;

    static const u32 IcrFixed = 0x0;
    static const u32 IcrLowest = 0x100;
    static const u32 IcrSmi = 0x200;
    static const u32 IcrNmi = 0x400;
    static const u32 IcrInit = 0x500;
    static const u32 IcrStartup = 0x600;
    static const u32 IcrPhysical = 0x0;
    static const u32 IcrLogical = 0x800;
    static const u32 IcrIdle = 0x0;
    static const u32 IcrSendPending = 0x1000;
    static const u32 IcrDeassert = 0x0;
    static const u32 IcrAssert = 0x4000;
    static const u32 IcrEdge = 0x0;
    static const u32 IcrLevel = 0x8000;
    static const u32 IcrNoShorthand = 0x0;
    static const u32 IcrSelf = 0x40000;
    static const u32 IcrAllIncludingSelf = 0x80000;
    static const u32 IcrAllExcludingSelf = 0xc0000;
    static const u32 IcrDestinationShift = 24;

    static const ulong BaseMsr = 0x1B;
    static const ulong BaseMsrGlobalEnable = (1UL << 11);
    static const ulong BaseMsrX2ApicEnable = (1UL << 10);

};

}