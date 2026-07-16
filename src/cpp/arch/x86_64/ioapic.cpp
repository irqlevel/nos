#include "ioapic.h"
#include <drivers/acpi.h>

#include "asm.h"
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
    /* High (destination) first: writing the low half first can briefly
       unmask the entry with a stale destination */
    WriteRegister(RedTbl + 2 * index + 1, (u32)(data >> 32));
    WriteRegister(RedTbl + 2 * index, (u32)data);
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

void IoApic::SetIrqDestination(u8 irq, u64 apicId)
{
    /* The destination id occupies bits 56-63 of the redirection entry,
       i.e. bits 24-31 of the high register. A single 32-bit write
       switches the destination without touching mask/trigger/vector. */
    u32 high = ReadRegister(RedTbl + 2 * irq + 1);
    high = (high & 0x00FFFFFFU) | ((u32)apicId << 24);
    WriteRegister(RedTbl + 2 * irq + 1, high);

    Trace(IoApicLL, "SetIrqDestination irq 0x%p apicId 0x%p",
        (ulong)irq, (ulong)apicId);
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