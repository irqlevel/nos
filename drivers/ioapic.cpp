#include "ioapic.h"
#include "acpi.h"

#include <kernel/trace.h>
#include <mm/mmio.h>

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
    SpinLock lock(OpLock);

    MmIo::Write32(Shared::MemAdd(BaseAddress, RegSel), reg);

    Barrier();

    return MmIo::Read32(Shared::MemAdd(BaseAddress, RegWin));
}

void IoApic::WriteRegister(u8 reg, u32 value)
{
    SpinLock lock(OpLock);

    MmIo::Write32(Shared::MemAdd(BaseAddress, RegSel), reg);

    Barrier();

    MmIo::Write32(Shared::MemAdd(BaseAddress, RegWin), value);
}

void IoApic::Enable()
{
    WriteRegister(ApicId, 0x0);

    u32 ver = ReadRegister(ApicVer);
    size_t pins = ((ver >> 16) & 0xFF) + 1;

    // Disable all entries
    for (size_t i = 0; i < pins; i++)
    {
        SetEntry(i, 1 << 16);
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
    data |= 0 << 8; // delivery mode: fixed
    data |= 0 << 11; // destination: physical
    data |= 0 << 12; // delivery status : relaxed
    data |= 0 << 13; // pin polarity: active high
    data |= 0 << 15; // trigger mode: edge
    data |= 0 << 16; // disable: no
    data |= apicId << 56; //destination id

    Trace(IoApicLL, "SetIrq irq 0x%p apicId 0x%p vector 0x%p data 0x%p",
        (ulong)irq, (ulong)apicId, (ulong)vector, (ulong)data);

    SetEntry(irq, data);
}

}