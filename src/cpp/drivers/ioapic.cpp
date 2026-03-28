#include "ioapic.h"
#include "acpi.h"

#include <kernel/asm.h>
#include <kernel/trace.h>
#include <mm/mmio.h>
#include <lib/lock.h>

namespace Kernel
{

IoApic::IoApic()
    : BaseAddress(nullptr)
{
    BaseAddress = Acpi::GetInstance().GetIoApicAddress();

    Trace(IoApicLL, "IO Apic addr 0x%p", BaseAddress);
}

IoApic::~IoApic()
{
}

u32 IoApic::ReadRegister(u8 reg)
{
    Stdlib::AutoLock lock(OpLock);

    Mm::MmIo::Write32(Stdlib::MemAdd(BaseAddress, RegSel), reg);

    Barrier();

    return Mm::MmIo::Read32(Stdlib::MemAdd(BaseAddress, RegWin));
}

void IoApic::WriteRegister(u8 reg, u32 value)
{
    Stdlib::AutoLock lock(OpLock);

    Mm::MmIo::Write32(Stdlib::MemAdd(BaseAddress, RegSel), reg);

    Barrier();

    Mm::MmIo::Write32(Stdlib::MemAdd(BaseAddress, RegWin), value);

    Barrier();
}

void IoApic::Enable()
{
    WriteRegister(ApicId, 0x0);

    u32 ver = ReadRegister(ApicVer);
    size_t pins = ((ver >> 16) & 0xFF) + 1;

    // Disable all entries
    for (size_t i = 0; i < pins; i++)
    {
        SetEntry(i, 1 << MaskedShift);
    }

    Trace(IoApicLL, "IO Apic ver 0x%p pins %u", ver, pins);
}

void IoApic::SetEntry(u8 index, u64 data)
{
    WriteRegister(RedTbl + 2 * index, (u32)data);
    WriteRegister(RedTbl + 2 * index + 1, (u32)(data >> 32));
}

void IoApic::SetIrq(u8 irq, u64 apicId, u8 vector)
{
    u64 data = 0;

    data |= vector; // interrupt vector
    data |= DmFixed << DelivModeShift; // delivery mode: fixed
    data |= 0 << DestModeShift; // destination: physical
    data |= 0 << DelivStatusShift; // delivery status : relaxed
    data |= 0 << PolarityShift; // pin polarity: active high
    data |= TriggerEdge << TriggerModeShift; // trigger mode: edge
    data |= 0 << MaskedShift; // disable: no
    data |= apicId << DestShift; //destination id

    Trace(IoApicLL, "SetIrq irq 0x%p apicId 0x%p vector 0x%p data 0x%p",
        (ulong)irq, (ulong)apicId, (ulong)vector, (ulong)data);

    SetEntry(irq, data);
}

void IoApic::SetIrqLevel(u8 irq, u64 apicId, u8 vector, bool activeHigh)
{
    u64 data = 0;

    data |= vector; // interrupt vector
    data |= DmFixed << DelivModeShift; // delivery mode: fixed
    data |= 0 << DestModeShift; // destination: physical
    data |= 0 << DelivStatusShift; // delivery status : relaxed
    data |= (activeHigh ? 0UL : 1UL) << PolarityShift; // pin polarity
    data |= TriggerLevel << TriggerModeShift; // trigger mode: level
    data |= 0 << MaskedShift; // disable: no
    data |= apicId << DestShift; //destination id

    Trace(IoApicLL, "SetIrqLevel irq 0x%p apicId 0x%p vector 0x%p data 0x%p activeHigh %u",
        (ulong)irq, (ulong)apicId, (ulong)vector, (ulong)data, (ulong)activeHigh);

    SetEntry(irq, data);
}

}