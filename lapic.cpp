#include "lapic.h"
#include "acpi.h"
#include "trace.h"
#include "asm.h"
#include "mmio.h"

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

void* Lapic::GetRegBase(ulong index)
{
    return Shared::MemAdd(BaseAddress, index * 0x10);
}

u32 Lapic::ReadReg(ulong index)
{
    SpinLock lock(OpLock);
    return MmIo::Read32(GetRegBase(index));
}

void Lapic::WriteReg(ulong index, u32 value)
{
    SpinLock lock(OpLock);
    MmIo::Write32(GetRegBase(index), value);
}

void Lapic::Enable()
{
    ulong msr = ReadMsr(BaseMsr);

    Trace(LapicLL, "Lapic: msr 0x%p", msr);

    WriteReg(TprIndex, 0x0);// Clear task priority to enable all interrupts
    WriteReg(DfrIndex, 0xffffffff);// Flat mode
    WriteReg(LdrIndex, 0x01000000);// All cpus use logical id 1
    WriteReg(SpIvIndex, 0x1FF);

    Trace(LapicLL, "Lapic: tpr 0x%p dfr 0x%p ldr 0x%p spiv 0x%p",
        (ulong)ReadReg(TprIndex), (ulong)ReadReg(DfrIndex), (ulong)ReadReg(LdrIndex), (ulong)ReadReg(SpIvIndex));

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