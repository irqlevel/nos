#include "lapic.h"
#include "acpi.h"
#include "trace.h"
#include "asm.h"

namespace Kernel
{

namespace Core
{

Lapic::Lapic()
    : BaseAddress(nullptr)
{
    BaseAddress = Acpi::GetInstance().GetLapicAddress();
}

Lapic::~Lapic()
{
}

volatile void* Lapic::GetRegBase(ulong index)
{
    return Shared::MemAdd(BaseAddress, index * 0x10);
}

u32 Lapic::ReadReg(ulong index)
{
    volatile u32* pReg = reinterpret_cast<volatile u32 *>(GetRegBase(index));

    u32 value = *pReg;
    Trace(LapicLL, "Lapic: read reg 0x%p val 0x%p", pReg, (ulong)value);
    return value;
}

void Lapic::WriteReg(ulong index, u32 value)
{
    volatile u32* pReg = reinterpret_cast<volatile u32 *>(GetRegBase(index));

    Trace(LapicLL, "Lapic: write reg 0x%p val 0x%p", pReg, (ulong)value);

    *pReg = value;
}

void Lapic::Enable()
{
    ulong msr = ReadMsr(BaseMsr);

    Trace(LapicLL, "Lapic: msr 0x%p", msr);
/*
    WriteReg(TprIndex, 0x0);// Clear task priority to enable all interrupts
    WriteReg(DfrIndex, 0xffffffff);// Flat mode
*/  
    WriteReg(SpuriousInterruptVectorIndex, 0x100);

    Trace(LapicLL, "Lapic: id 0x%p", (ulong)GetId());
}

bool Lapic::CheckIsr(u8 vector)
{
    ulong isrRegNumber = vector / 32;
    u8 regOffset = vector % 32;

    return (ReadReg(IsrBaseIndex + isrRegNumber) & (1 << regOffset)) ? true : false;
}

void Lapic::EOI(u8 vector)
{
    if (CheckIsr(vector))
        WriteReg(EoiIndex, 0x1);
}

u8 Lapic::GetId()
{
    return ReadReg(ApicIdIndex) >> 24;
}

}

}