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
}

Acpi::~Acpi()
{
}

bool Acpi::CheckSum(void* table, size_t len)
{
    u8* p = reinterpret_cast<u8*>(table);

    u8 sum = 0;
    for (size_t i = 0; i < len; i++)
    {
        sum += p[i];
    }

    return (sum) ? false : true;
}

bool Acpi::ParseRsdp(RSDPDescriptor20 *rsdp)
{
    if (!CheckSum(rsdp, sizeof(rsdp->FirstPart)))
    {
        Trace(AcpiLL, "Rsdp 0x%p checksum failed", rsdp);
        return false;
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

bool Acpi::ParseRsdt(ACPISDTHeader* rsdt)
{
    if (!CheckSum(rsdt, sizeof(rsdt->Length)))
    {
        Trace(AcpiLL, "Rsdt 0x%p checksum failed", rsdt);
        return false;
    }

    return true;
}

bool Acpi::Parse()
{
    RSDPDescriptor20* rsdp = FindRsdp();
    if (rsdp == nullptr)
    {
        return false;
    }

    Rsdp = rsdp;
    ACPISDTHeader* rsdt = reinterpret_cast<ACPISDTHeader*>(Rsdp->FirstPart.RsdtAddress);
    if (!ParseRsdt(rsdt))
    {
        return false;
    }
    Rsdt = rsdt;

    return true;
}

}
}