#include "ioapic.h"
#include "acpi.h"
#include "trace.h"

namespace Kernel
{

namespace Core
{

IoApic::IoApic()
    : BaseAddress(nullptr)
{
    BaseAddress = Acpi::GetInstance().GetIoApicAddress();
}

IoApic::~IoApic()
{
}

u32 IoApic::ReadRegister(u8 reg)
{
    *reinterpret_cast<volatile u32*>(Shared::MemAdd(BaseAddress, RegSel)) = reg;
    return *reinterpret_cast<volatile u32*>(Shared::MemAdd(BaseAddress, RegWin));
}

void IoApic::WriteRegister(u8 reg, u8 value)
{
    *reinterpret_cast<volatile u32*>(Shared::MemAdd(BaseAddress, RegSel)) = reg;
    *reinterpret_cast<volatile u32*>(Shared::MemAdd(BaseAddress, RegWin)) = value;
}

void IoApic::Enable()
{
    WriteRegister(ApicId, 0x0);

    u32 x = ReadRegister(ApicVer);
    size_t numIrqs = ((x >> 16) & 0xFF) + 1;

    // Disable all entries
    for (size_t i = 0; i < numIrqs; i++)
    {
        SetEntry(i, 1 << 16);
    }

    Trace(IoApicLL, "IO Apic numIrqs %u", numIrqs);
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
    data |= 0x0 << 8; // delivery mode: fixed
    data |= 0 << 11; // destination: physical
    data |= 0 << 12; // delivery status : relaxed
    data |= 0 << 13; // pin polarity: active high
    data |= 0 << 15; // trigger mode: edge
    data |= 0 << 16; // disable: no
    data |= apicId << 56; //destination id

    SetEntry(irq, data);
}

}

}