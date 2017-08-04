#include "acpi.h"

#include <kernel/trace.h>
#include <kernel/cpu.h>

namespace Kernel
{

Acpi::Acpi()
    : Rsdp(nullptr)
    , Rsdt(nullptr)
    , LapicAddress(nullptr)
    , IoApicAddress(nullptr)
    , IrqToGsiSize(0)
{
    OemId[0] = '\0';
    for (size_t i = 0; i < Stdlib::ArraySize(Table); i++)
    {
        Table[i] = nullptr;
    }
}

Acpi::~Acpi()
{
}

int Acpi::ComputeSum(void* table, size_t len)
{
    u8* p = reinterpret_cast<u8*>(table);

    ulong sum = 0;
    for (size_t i = 0; i < len; i++)
    {
        sum += *p++;
    }

    return sum & 0xFF;
}

bool Acpi::ParseRsdp(RSDPDescriptor20 *rsdp)
{
    if (ComputeSum(rsdp, sizeof(rsdp->FirstPart)) != 0)
    {
        Trace(AcpiLL, "Rsdp 0x%p checksum failed 0x%p vs 0x%p",
            rsdp, (ulong)ComputeSum(rsdp, sizeof(rsdp->FirstPart)));
    }

    Stdlib::MemCpy(OemId, rsdp->FirstPart.OEMID, sizeof(rsdp->FirstPart.OEMID));

    Trace(AcpiLL, "Rsdp 0x%p revision %u OemId %s Rsdt 0x%p",
        rsdp, (ulong)rsdp->FirstPart.Revision, OemId, (ulong)rsdp->FirstPart.RsdtAddress);

    return true;
}

Acpi::RSDPDescriptor20* Acpi::FindRsdp()
{
    // Search main BIOS area below 1MB
    // TODO - Search Extended BIOS Area
    u8 *p = reinterpret_cast<u8 *>(0x000e0000);
    u8 *end = reinterpret_cast<u8 *>(0x000fffff);
    while (p < end)
    {
        RSDPDescriptor20 *rsdp = reinterpret_cast<RSDPDescriptor20*>(p);
        if (rsdp->FirstPart.Signature == RSDPSignature)
        {
            if (ParseRsdp(rsdp))
            {
                return rsdp;
            }
        }
        p += 16;
    }

    return nullptr;
}

Stdlib::Error Acpi::ParseRsdt(ACPISDTHeader* rsdt)
{
    if (Stdlib::StrnCmp(rsdt->Signature, "RSDT", sizeof(rsdt->Signature)) != 0)
    {
        Trace(AcpiLL, "Rsdt 0x%p invalid signature", rsdt);
        return MakeError(Stdlib::Error::NotFound);
    }

    if (checkRsdtChecksum)
    {
        if (ComputeSum(rsdt, sizeof(rsdt->Length)) != 0)
        {
            Trace(AcpiLL, "Rsdt 0x%p checksum failed 0x%p vs 0x%p",
                rsdt, (ulong)ComputeSum(rsdt, sizeof(rsdt->Length)), (ulong)rsdt->Checksum);
             return MakeError(Stdlib::Error::NotFound);
        }
    }

    return MakeError(Stdlib::Error::Success);
}

Acpi::ACPISDTHeader* Acpi::LookupTable(const char *name)
{
    if (Stdlib::StrLen(name) != 4)
    {
        return nullptr;
    }

    for (size_t i = 0; i < Stdlib::ArraySize(Table); i++)
    {
        if (Table[i] != nullptr && Stdlib::StrnCmp(Table[i]->Signature, name, 4) == 0)
        {
            return Table[i];
        }
    }

    return nullptr;
}

Stdlib::Error Acpi::ParseTablePointers()
{
    Stdlib::Error err;

    if (Rsdt->Length <= sizeof(*Rsdt))
        return MakeError(Stdlib::Error::NotFound);

    size_t tableCount = (Rsdt->Length - OFFSET_OF(ACPISDTHeader, Entry)) / sizeof(Rsdt->Entry[0]);
    Trace(AcpiLL, "Acpi: tableCount %u", tableCount);

    for (size_t i = 0; i < tableCount; i++)
    {
        ACPISDTHeader* header = reinterpret_cast<ACPISDTHeader*>(Rsdt->Entry[i]);
        char tableSignature[5];

        Stdlib::MemCpy(tableSignature, header->Signature, sizeof(header->Signature));
        tableSignature[4] = '\0';

        Trace(AcpiLL, "Acpi: table 0x%p %s", header, tableSignature);
        if (i >= Stdlib::ArraySize(Table))
        {
            Trace(0, "Acpi: can't insert table %u", (ulong)i);
            return MakeError(Stdlib::Error::NotFound);
        }

        Table[i] = header;
        header++;
    }

     return MakeError(Stdlib::Error::Success);
}

Stdlib::Error Acpi::ParseMADT()
{
    ACPISDTHeader* sdtHeader = LookupTable("APIC");
    if (sdtHeader == nullptr)
    {
        return MakeError(Stdlib::Error::NotFound);
    }

    Trace(AcpiLL, "Acpi: MADT 0x%p", sdtHeader);

    MadtHeader* header = reinterpret_cast<MadtHeader*>(sdtHeader + 1);
    Trace(AcpiLL, "Acpi: MADT LIntCtrl 0x%p flags 0x%p",
        (ulong)header->LocalIntCtrlAddress, (ulong)header->Flags);

    LapicAddress = reinterpret_cast<void*>((ulong)header->LocalIntCtrlAddress);

    MadtEntry* entry = &header->Entry[0];

    while (Stdlib::MemAdd(entry, entry->Length) <= Stdlib::MemAdd(sdtHeader, sdtHeader->Length))
    {
        Trace(AcpiLL, "Acpi: MADT entry 0x%p type %u len %u",
            entry, (ulong)entry->Type, (ulong)entry->Length);

        if (entry->Length == 0)
        {
            break;
        }

        switch (entry->Type)
        {
        case MadtEntryTypeLapic:
        {
            MadtLapicEntry* lapicEntry = reinterpret_cast<MadtLapicEntry*>(entry + 1);
            if (entry->Length < sizeof(*lapicEntry) + sizeof(*entry))
                return MakeError(Stdlib::Error::InvalidValue);

            Trace(AcpiLL, "Acpi: MADT lapic procId %u apicId %u flags 0x%p",
                (ulong)lapicEntry->AcpiProcessId, (ulong)lapicEntry->ApicId, (ulong)lapicEntry->Flags);

            if (lapicEntry->Flags & 0x1)
            {
                if (!CpuTable::GetInstance().InsertCpu(lapicEntry->ApicId))
                    return MakeError(Stdlib::Error::Unsuccessful);
            }
            break;
        }
        case MadtEntryTypeIoApic:
        {
            MadtIoApicEntry* ioApicEntry = reinterpret_cast<MadtIoApicEntry*>(entry + 1);
            if (entry->Length < sizeof(*ioApicEntry) + sizeof(*entry))
                return MakeError(Stdlib::Error::InvalidValue);

            IoApicAddress = reinterpret_cast<void*>((ulong)ioApicEntry->IoApicAddress);

            Trace(AcpiLL, "Acpi: MADT ioApicId %u addr 0x%p gsi 0x%p",
                (ulong)ioApicEntry->IoApicId, (ulong)ioApicEntry->IoApicAddress,
                (ulong)ioApicEntry->GlobalSystemInterruptBase);
            break;
        }
        case MadtEntryTypeIntSrcOverride:
        {
            MadtIntSrcOverrideEntry* isoEntry = reinterpret_cast<MadtIntSrcOverrideEntry*>(entry + 1);
            if (entry->Length < sizeof(*isoEntry) + sizeof(*entry))
                return MakeError(Stdlib::Error::InvalidValue);

            Trace(AcpiLL, "Acpi: MADT bus 0x%p irq 0x%p gsi 0x%p flags 0x%p",
                (ulong)isoEntry->BusSource, (ulong)isoEntry->IrqSource, (ulong)isoEntry->GlobalSystemInterrupt,
                (ulong)isoEntry->Flags);

            if (!RegisterIrqToGsi(isoEntry->IrqSource, isoEntry->GlobalSystemInterrupt))
                return MakeError(Stdlib::Error::NoMemory);

            break;
        }
        default:
            break;
        }

        entry = static_cast<MadtEntry*>(Stdlib::MemAdd(entry, entry->Length));
    }

    return MakeError(Stdlib::Error::Success);
}

Stdlib::Error Acpi::Parse()
{
    Stdlib::Error err;
    RSDPDescriptor20* rsdp = FindRsdp();
    if (rsdp == nullptr)
    {
        return MakeError(Stdlib::Error::NotFound);
    }

    Rsdp = rsdp;
    ACPISDTHeader* rsdt = reinterpret_cast<ACPISDTHeader*>(Rsdp->FirstPart.RsdtAddress);
    err = ParseRsdt(rsdt);
    if (!err.Ok())
    {
        return err;
    }
    Rsdt = rsdt;

    err = ParseTablePointers();
    if (!err.Ok())
    {
        return err;
    }

    err = ParseMADT();
    if (!err.Ok())
    {
        return err;
    }

    return MakeError(Stdlib::Error::Success);
}


void* Acpi::GetLapicAddress()
{
    return LapicAddress;
}

void* Acpi::GetIoApicAddress()
{
    return IoApicAddress;
}

bool Acpi::RegisterIrqToGsi(u8 irq, u32 gsi)
{
    if (IrqToGsiSize >= Stdlib::ArraySize(IrqToGsi))
        return false;

    auto& entry = IrqToGsi[IrqToGsiSize];
    entry.Irq = irq;
    entry.Gsi = gsi;
    IrqToGsiSize++;
    return true;
}

u32 Acpi::GetGsiByIrq(u8 irq)
{
    for (size_t i = 0; i < IrqToGsiSize; i++)
    {
        auto& entry = IrqToGsi[i];
        if (entry.Irq == irq)
        {
            return entry.Gsi;
        }
    }

    return irq;
}

}