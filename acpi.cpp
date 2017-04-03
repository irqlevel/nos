#include "acpi.h"
#include "trace.h"

namespace Kernel
{

namespace Core
{

Acpi::Acpi()
{
}

Acpi::~Acpi()
{
}


bool Acpi::ParseRsdp(RSDPDescriptor20 *rsdp)
{
    u8* p = reinterpret_cast<u8*>(rsdp);
    u8 sum = 0;
    for (size_t i = 0; i < sizeof(rsdp->FirstPart); i++)
    {
        sum += p[i];
    }

    if (sum)
    {
        Trace(AcpiLL, "Rsdp 0x%p checksum failed", rsdp);
        return false;
    }

    Trace(AcpiLL, "Rsdp 0x%p revision %u", rsdp, (ulong)rsdp->FirstPart.Revision);

    return true;
}

void* Acpi::GetRsdp()
{
    // Search main BIOS area below 1MB
    // TODO - Search Extended BIOS Area
    u8 *p = reinterpret_cast<u8 *>(0x000e0000);
    u8 *end = reinterpret_cast<u8 *>(0x000fffff);

    while (p < end)
    {
        RSDPDescriptor20 *desc = reinterpret_cast<RSDPDescriptor20*>(p);
        if (desc->FirstPart.Signature == RSDPSignature)
        {
            if (ParseRsdp(desc))
            {
                return desc;
            }
        }

        p += 16;
    }

    return nullptr;
}

}
}