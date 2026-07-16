#include "lapic.h"
#include <drivers/acpi.h>

#include <kernel/trace.h>
#include <kernel/asm.h>
#include <mm/mmio.h>

namespace Kernel
{

void* Lapic::GetRegBase(ulong index)
{
    return Stdlib::MemAdd(Acpi::GetInstance().GetLapicAddress(), index * 0x10);
}

u32 Lapic::ReadReg(ulong index)
{
    Barrier();
    return Mm::MmIo::Read32(GetRegBase(index));
}

void Lapic::WriteReg(ulong index, u32 value)
{
    Mm::MmIo::Write32(GetRegBase(index), value);
    Barrier();
}

void Lapic::Enable()
{
    ulong msr = ReadMsr(BaseMsr);

    Trace(LapicLL, "Lapic: msr 0x%p base 0x%p", msr, Acpi::GetInstance().GetLapicAddress());

    /* Firmware may hand off with the APIC in x2APIC mode, where the
       MMIO window is dead. The SDM forbids switching x2APIC->xAPIC
       directly: go through the globally-disabled state first. */
    if (msr & BaseMsrX2ApicEnable)
    {
        Trace(0, "Lapic: firmware left x2APIC mode, switching to xAPIC");
        WriteMsr(BaseMsr, msr & ~(BaseMsrX2ApicEnable | BaseMsrGlobalEnable));
        msr = msr & ~(BaseMsrX2ApicEnable | BaseMsrGlobalEnable);
    }

    /* Make sure the APIC is globally enabled (xAPIC MMIO mode) */
    if (!(msr & BaseMsrGlobalEnable))
    {
        msr = msr | BaseMsrGlobalEnable;
        WriteMsr(BaseMsr, msr);
    }

    WriteReg(DfrIndex, 0xffffffff);// Flat mode
    WriteReg(LdrIndex, 0x01000000);// All cpus use logical id 1
    WriteReg(TprIndex, 0xFF);// Disable all interrupts
    WriteReg(SpIvIndex, 0x100 | SpuriousVector);// bit 8 = APIC software enable

    Trace(LapicLL, "Lapic: tpr 0x%p dfr 0x%p ldr 0x%p spiv 0x%p",
        (ulong)ReadReg(TprIndex), (ulong)ReadReg(DfrIndex), (ulong)ReadReg(LdrIndex), (ulong)ReadReg(SpIvIndex));

    Trace(LapicLL, "Lapic: apicId 0x%p", (ulong)GetApicId());

    WriteReg(EoiIndex, 0x0); // Acknowledge any outstanding interrupts

    WriteReg(TprIndex, 0x0);// Clear task priority to enable all interrupts
}

bool Lapic::CheckIsr(u8 vector)
{
    ulong isrRegNumber = vector / 32;
    u8 regOffset = vector % 32;

    return (ReadReg(IsrBaseIndex + isrRegNumber) & (1 << regOffset)) ? true : false;
}

void Lapic::EOI()
{
    WriteReg(EoiIndex, 0x0);
}

void Lapic::EOI(u8 vector)
{
    if (vector != SpuriousVector)
        WriteReg(EoiIndex, 0x0);
}

u8 Lapic::GetApicId()
{
    return ReadReg(ApicIdIndex) >> 24;
}

void Lapic::SendInit(u32 apicId)
{
    /* Same ICR interleaving hazard as SendIPI below */
    ulong flags = GetRflags();
    InterruptDisable();

    WriteReg(IcrHighIndex, apicId << IcrDestinationShift);
    WriteReg(IcrLowIndex, IcrInit | IcrPhysical | IcrAssert | IcrEdge | IcrNoShorthand);

    while (ReadReg(IcrLowIndex) & IcrSendPending)
    {
        Pause();
    }

    SetRflags(flags);
}

void Lapic::SendStartup(u32 apicId, u32 vector)
{
    /* Same ICR interleaving hazard as SendIPI below */
    ulong flags = GetRflags();
    InterruptDisable();

    WriteReg(IcrHighIndex, apicId << IcrDestinationShift);
    WriteReg(IcrLowIndex, vector | IcrStartup | IcrPhysical | IcrAssert | IcrEdge | IcrNoShorthand);

    while (ReadReg(IcrLowIndex) & IcrSendPending)
    {
        Pause();
    }

    SetRflags(flags);
}

void Lapic::SendIPI(u32 apicId, u32 vector)
{
    /* The two ICR writes must not be interleaved with another SendIPI
       on this CPU (e.g. from an interrupt handler): the second sender
       would clobber IcrHigh and redirect the first IPI. */
    ulong flags = GetRflags();
    InterruptDisable();

    WriteReg(IcrHighIndex, apicId << IcrDestinationShift);
    WriteReg(IcrLowIndex, vector | IcrPhysical | IcrAssert | IcrEdge | IcrNoShorthand);

    while (ReadReg(IcrLowIndex) & IcrSendPending)
    {
        Pause();
    }

    SetRflags(flags);
}

}
