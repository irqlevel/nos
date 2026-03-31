#include "acpi.h"

#include <kernel/trace.h>
#include <kernel/cpu.h>
#include <mm/memory_map.h>
#include <mm/page_table.h>

namespace Kernel
{

Acpi::Acpi()
    : Rsdt(nullptr)
    , LapicAddress(nullptr)
    , IoApicAddress(nullptr)
    , IrqToGsiSize(0)
    , Pm1aCntPort(0)
    , ResetRegValid(false)
    , ResetRegPort(0)
    , ResetVal(0)
    , HpetBasePhys(0)
    , HpetMinTick(0)
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
        Trace(0, "Rsdp 0x%p checksum failed (sum 0x%p)",
            rsdp, (ulong)ComputeSum(rsdp, sizeof(rsdp->FirstPart)));
        return false;
    }

    Stdlib::MemCpy(OemId, rsdp->FirstPart.OEMID, sizeof(rsdp->FirstPart.OEMID));
    OemId[sizeof(rsdp->FirstPart.OEMID)] = '\0';

    Trace(0, "Rsdp 0x%p revision %u OemId %s Rsdt 0x%p",
        rsdp, (ulong)rsdp->FirstPart.Revision, OemId, (ulong)rsdp->FirstPart.RsdtAddress);

    return true;
}

Acpi::RSDPDescriptor20* Acpi::FindRsdp()
{
    auto& mm = Kernel::Mm::MemoryMap::GetInstance();
    auto& pt = Kernel::Mm::PageTable::GetInstance();

    for (size_t i = 0; i < mm.GetRegionCount(); i++)
    {
        ulong addr, len, type;

        if (!mm.GetRegion(i, addr, len, type))
            return nullptr;

        /* Type 1 = usable RAM, type 2 = reserved (e.g. BIOS/ACPI at 0xE0000-0xFFFFF). */
        if (type != 1 && type != 2)
            continue;

        ulong pageStart = Stdlib::RoundDown(addr, Const::PageSize);
        for (ulong curr = pageStart; curr < (addr + len); curr+= Const::PageSize)
        {
            ulong pageVa = pt.TmpMapPage(curr);
            if (!pageVa)
            {
                Trace(0, "Can't map 0x%p", curr);
                return nullptr;
            }

            for (ulong va = pageVa; va < (pageVa + Const::PageSize); va += 16)
            {
                RSDPDescriptor20 *rsdp = reinterpret_cast<RSDPDescriptor20*>(va);
                if (rsdp->FirstPart.Signature == RSDPSignature)
                {
                    Trace(0, "Checking rsdp va 0x%p pha 0x%p", rsdp, curr + (va - pageVa));
                    if (ParseRsdp(rsdp))
                    {
                        return rsdp;
                    }
                }
            }
            pt.TmpUnmapPage(pageVa);
        }
    }

    Trace(0, "Rsdp not found");
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
        if (ComputeSum(rsdt, rsdt->Length) != 0)
        {
            Trace(AcpiLL, "Rsdt 0x%p checksum failed 0x%p vs 0x%p",
                rsdt, (ulong)ComputeSum(rsdt, rsdt->Length), (ulong)rsdt->Checksum);
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

    auto& pt = Mm::PageTable::GetInstance();

    for (size_t i = 0; i < tableCount; i++)
    {
        if (i >= Stdlib::ArraySize(Table))
        {
            Trace(0, "Acpi: can't insert table %u", (ulong)i);
            return MakeError(Stdlib::Error::NotFound);
        }

        /*
         * First map just the header page to read Length, then re-map
         * the full table so that parsers have a contiguous VA range.
         */
        ACPISDTHeader* header = reinterpret_cast<ACPISDTHeader*>(pt.TmpMapAddress(Rsdt->Entry[i]));
        if (!header)
        {
            Trace(0, "Acpi: can't map table %u phys 0x%p", (ulong)i, (ulong)Rsdt->Entry[i]);
            return MakeError(Stdlib::Error::NoMemory);
        }

        u32 tableLength = header->Length;
        if (tableLength < sizeof(ACPISDTHeader))
        {
            Trace(0, "Acpi: table %u length %u too small", (ulong)i, (ulong)tableLength);
            return MakeError(Stdlib::Error::InvalidValue);
        }

        ulong physOffset = Rsdt->Entry[i] & (Const::PageSize - 1);
        if (physOffset + tableLength > Const::PageSize)
        {
            /* Table spans pages — unmap the single-page mapping and re-map the full range */
            pt.TmpUnmapPage(reinterpret_cast<ulong>(header) & ~(Const::PageSize - 1));
            header = reinterpret_cast<ACPISDTHeader*>(pt.TmpMapRange(Rsdt->Entry[i], tableLength));
            if (!header)
            {
                Trace(0, "Acpi: can't map table %u range phys 0x%p len %u",
                    (ulong)i, (ulong)Rsdt->Entry[i], (ulong)tableLength);
                return MakeError(Stdlib::Error::NoMemory);
            }
        }

        char tableSignature[5];
        Stdlib::MemCpy(tableSignature, header->Signature, sizeof(header->Signature));
        tableSignature[4] = '\0';

        Trace(AcpiLL, "Acpi: table 0x%p %s len %u", header, tableSignature, (ulong)tableLength);

        Table[i] = header;
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

    LapicAddress = (void *)Mm::PageTable::GetInstance().TmpMapAddress(header->LocalIntCtrlAddress);
    if (LapicAddress == nullptr)
    {
        return MakeError(Stdlib::Error::NoMemory);
    }

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
            if (entry->Length < sizeof(MadtLapicEntry) + sizeof(*entry))
                return MakeError(Stdlib::Error::InvalidValue);
            MadtLapicEntry* lapicEntry = reinterpret_cast<MadtLapicEntry*>(entry + 1);

            Trace(AcpiLL, "Acpi: MADT lapic procId %u apicId %u flags 0x%p",
                (ulong)lapicEntry->AcpiProcessId, (ulong)lapicEntry->ApicId, (ulong)lapicEntry->Flags);

            if (lapicEntry->Flags & 0x1)
            {
                if (!CpuTable::GetInstance().InsertCpu(lapicEntry->ApicId))
                {
                    Trace(AcpiLL, "Acpi: MADT lapic apicId %u ignored (max %u)",
                        (ulong)lapicEntry->ApicId, (ulong)MaxCpus);
                }
            }
            break;
        }
        case MadtEntryTypeIoApic:
        {
            if (entry->Length < sizeof(MadtIoApicEntry) + sizeof(*entry))
                return MakeError(Stdlib::Error::InvalidValue);
            MadtIoApicEntry* ioApicEntry = reinterpret_cast<MadtIoApicEntry*>(entry + 1);

            IoApicAddress = (void *)Mm::PageTable::GetInstance().TmpMapAddress(ioApicEntry->IoApicAddress);
            if (IoApicAddress == nullptr)
            {
                return MakeError(Stdlib::Error::NoMemory);
            }

            Trace(AcpiLL, "Acpi: MADT ioApicId %u addr 0x%p gsi 0x%p",
                (ulong)ioApicEntry->IoApicId, (ulong)ioApicEntry->IoApicAddress,
                (ulong)ioApicEntry->GlobalSystemInterruptBase);
            break;
        }
        case MadtEntryTypeIntSrcOverride:
        {
            if (entry->Length < sizeof(MadtIntSrcOverrideEntry) + sizeof(*entry))
                return MakeError(Stdlib::Error::InvalidValue);
            MadtIntSrcOverrideEntry* isoEntry = reinterpret_cast<MadtIntSrcOverrideEntry*>(entry + 1);

            Trace(AcpiLL, "Acpi: MADT bus 0x%p irq 0x%p gsi 0x%p flags 0x%p",
                (ulong)isoEntry->BusSource, (ulong)isoEntry->IrqSource, (ulong)isoEntry->GlobalSystemInterrupt,
                (ulong)isoEntry->Flags);

            if (!RegisterIrqToGsi(isoEntry->IrqSource, isoEntry->GlobalSystemInterrupt, isoEntry->Flags))
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

void Acpi::ParseFADT()
{
    ACPISDTHeader* sdtHeader = LookupTable("FACP");
    if (sdtHeader == nullptr)
    {
        Trace(AcpiLL, "Acpi: no FADT table");
        return;
    }

    ulong bodyLen = sdtHeader->Length - sizeof(ACPISDTHeader);
    FadtFields* fadt = reinterpret_cast<FadtFields*>(sdtHeader + 1);

    /* Pm1aCntBlk sits at body offset +28; need at least 32 bytes of body */
    static const ulong Pm1aCntBlkEnd = OFFSET_OF(FadtFields, Pm1aCntBlk) + sizeof(fadt->Pm1aCntBlk);
    if (bodyLen >= Pm1aCntBlkEnd)
    {
        Pm1aCntPort = fadt->Pm1aCntBlk;
        Trace(AcpiLL, "Acpi: FADT PM1a_CNT port 0x%p", Pm1aCntPort);
    }

    /* Flags + ResetReg + ResetValue require at least 93 bytes of body (ACPI 2.0+) */
    static const ulong ResetValueEnd = OFFSET_OF(FadtFields, ResetValue) + sizeof(fadt->ResetValue);
    if (bodyLen >= ResetValueEnd)
    {
        Trace(AcpiLL, "Acpi: FADT flags 0x%p", (ulong)fadt->Flags);

        /* RESET_REG_SUP is bit 10 of Flags */
        static const u32 ResetRegSup = (1u << 10);
        if ((fadt->Flags & ResetRegSup) && fadt->ResetReg.AddressSpaceId == 1 /* I/O */)
        {
            ResetRegValid = true;
            ResetRegPort = (ulong)fadt->ResetReg.Address;
            ResetVal = fadt->ResetValue;
            Trace(AcpiLL, "Acpi: FADT RESET_REG port 0x%p value 0x%p",
                ResetRegPort, (ulong)ResetVal);
        }
    }
}

void Acpi::ParseHPET()
{
    ACPISDTHeader* sdtHeader = LookupTable("HPET");
    if (sdtHeader == nullptr)
    {
        Trace(AcpiLL, "Acpi: no HPET table");
        return;
    }

    if (sdtHeader->Length < sizeof(ACPISDTHeader) + sizeof(HpetTableBody))
    {
        Trace(0, "Acpi: HPET table too short: %u", (ulong)sdtHeader->Length);
        return;
    }

    HpetTableBody* hpet = reinterpret_cast<HpetTableBody*>(sdtHeader + 1);

    /* BaseAddress must be system memory (AddressSpaceId == 0) */
    if (hpet->BaseAddress.AddressSpaceId != 0)
    {
        Trace(0, "Acpi: HPET base not in system memory (id %u)", (ulong)hpet->BaseAddress.AddressSpaceId);
        return;
    }

    HpetBasePhys = (ulong)hpet->BaseAddress.Address;
    HpetMinTick  = hpet->MinimumClockTick;

    Trace(AcpiLL, "Acpi: HPET base 0x%p minTick %u blockId 0x%p",
        HpetBasePhys, (ulong)HpetMinTick, (ulong)hpet->EventTimerBlockId);
}

Stdlib::Error Acpi::Parse()
{
    Stdlib::Error err;
    RSDPDescriptor20* rsdp = FindRsdp();
    if (rsdp == nullptr)
    {
        return MakeError(Stdlib::Error::NotFound);
    }

    /* Extract RsdtAddress and free the RSDP temp-map slot */
    u32 rsdtPhysAddr = rsdp->FirstPart.RsdtAddress;
    auto& pt = Mm::PageTable::GetInstance();
    pt.TmpUnmapPage(reinterpret_cast<ulong>(rsdp) & ~(Const::PageSize - 1));

    ACPISDTHeader* rsdt = reinterpret_cast<ACPISDTHeader*>(pt.TmpMapAddress(rsdtPhysAddr));
    if (!rsdt)
        return MakeError(Stdlib::Error::NoMemory);

    err = ParseRsdt(rsdt);
    if (!err.Ok())
    {
        return err;
    }

    /* Re-map the full RSDT if it spans a page boundary */
    ulong rsdtPhysOff = rsdtPhysAddr & (Const::PageSize - 1);
    u32 rsdtLength = rsdt->Length;
    if (rsdtPhysOff + rsdtLength > Const::PageSize)
    {
        pt.TmpUnmapPage(reinterpret_cast<ulong>(rsdt) & ~(Const::PageSize - 1));
        rsdt = reinterpret_cast<ACPISDTHeader*>(pt.TmpMapRange(rsdtPhysAddr, rsdtLength));
        if (!rsdt)
            return MakeError(Stdlib::Error::NoMemory);
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

    ParseFADT();
    ParseHPET();

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

bool Acpi::RegisterIrqToGsi(u8 irq, u32 gsi, u16 flags)
{
    if (IrqToGsiSize >= Stdlib::ArraySize(IrqToGsi))
        return false;

    auto& entry = IrqToGsi[IrqToGsiSize];
    entry.Irq = irq;
    entry.Gsi = gsi;
    entry.Flags = flags;
    IrqToGsiSize++;
    return true;
}

u16 Acpi::GetIrqFlags(u8 irq)
{
    for (size_t i = 0; i < IrqToGsiSize; i++)
    {
        auto& entry = IrqToGsi[i];
        if (entry.Irq == irq)
            return entry.Flags;
    }
    return 0; /* Default: conforms to bus specification */
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

ulong Acpi::GetPm1aCntPort()
{
    return Pm1aCntPort;
}

bool Acpi::HasResetReg()
{
    return ResetRegValid;
}

ulong Acpi::GetResetRegPort()
{
    return ResetRegPort;
}

u8 Acpi::GetResetValue()
{
    return ResetVal;
}

ulong Acpi::GetHpetBasePhys()
{
    return HpetBasePhys;
}

u16 Acpi::GetHpetMinTick()
{
    return HpetMinTick;
}

}