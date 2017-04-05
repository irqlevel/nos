#include "acpi.h"
#include "trace.h"

namespace Kernel
{

namespace Core
{

Acpi::Acpi()
    : Rsdp(nullptr)
    , Rsdt(nullptr)
{
    OemId[0] = '\0';
    for (size_t i = 0; i < Shared::ArraySize(Table); i++)
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

    Shared::MemCpy(OemId, rsdp->FirstPart.OEMID, sizeof(rsdp->FirstPart.OEMID));

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

Shared::Error Acpi::ParseRsdt(ACPISDTHeader* rsdt)
{
    if (Shared::StrnCmp(rsdt->Signature, "RSDT", sizeof(rsdt->Signature)) != 0)
    {
        Trace(AcpiLL, "Rsdt 0x%p invalid signature", rsdt);
        return MakeError(Shared::Error::NotFound);
    }

    if (checkRsdtChecksum)
    {
        if (ComputeSum(rsdt, sizeof(rsdt->Length)) != 0)
        {
            Trace(AcpiLL, "Rsdt 0x%p checksum failed 0x%p vs 0x%p",
                rsdt, (ulong)ComputeSum(rsdt, sizeof(rsdt->Length)), (ulong)rsdt->Checksum);
             return MakeError(Shared::Error::NotFound);
        }
    }

    return MakeError(Shared::Error::Success);
}

Acpi::ACPISDTHeader* Acpi::LookupTable(const char *name)
{
    if (Shared::StrLen(name) != 4)
    {
        return nullptr;
    }

    for (size_t i = 0; i < Shared::ArraySize(Table); i++)
    {
        if (Table[i] != nullptr && Shared::StrnCmp(Table[i]->Signature, name, 4) == 0)
        {
            return Table[i];
        }
    }

    return nullptr;
}

Shared::Error Acpi::ParseTablePointers()
{
    Shared::Error err;

    if (Rsdt->Length <= sizeof(*Rsdt))
        return MakeError(Shared::Error::NotFound);

    size_t tableCount = (Rsdt->Length - OFFSET_OF(ACPISDTHeader, Entry)) / sizeof(Rsdt->Entry[0]);
    Trace(0, "Acpi: tableCount %u", tableCount);

    for (size_t i = 0; i < tableCount; i++)
    {
        ACPISDTHeader* header = reinterpret_cast<ACPISDTHeader*>(Rsdt->Entry[i]);
        char tableSignature[5];

        Shared::MemCpy(tableSignature, header->Signature, sizeof(header->Signature));
        tableSignature[4] = '\0';

        Trace(0, "Acpi: table 0x%p %s", header, tableSignature);
        if (i >= Shared::ArraySize(Table))
        {
            Trace(0, "Acpi: can't insert table %u", (ulong)i);
            return MakeError(Shared::Error::NotFound);
        }

        Table[i] = header;
        header++;
    }

     return MakeError(Shared::Error::Success);
}

Shared::Error Acpi::ParseMADT()
{
    ACPISDTHeader* table = LookupTable("APIC");
    if (table == nullptr)
    {
        return MakeError(Shared::Error::NotFound);
    }
    Trace(0, "Acpi: MADT 0x%p", table);
    return MakeError(Shared::Error::Success);
}

Shared::Error Acpi::Parse()
{
    Shared::Error err;
    RSDPDescriptor20* rsdp = FindRsdp();
    if (rsdp == nullptr)
    {
        return MakeError(Shared::Error::NotFound);
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

    return MakeError(Shared::Error::Success);
}

}
}