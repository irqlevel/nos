#include "acpi.h"

#include <kernel/trace.h>
#include <kernel/cpu.h>
#include <mm/memory_map.h>
#include <mm/page_table.h>
#include <arch/x86_64/grub.h>

namespace Kernel
{

Acpi::Acpi()
    : Root(nullptr)
    , RootIsXsdt(false)
    , LapicAddress(nullptr)
    , IoApicAddress(nullptr)
    , IrqToGsiSize(0)
    , Pm1aCntPort(0)
    , ResetRegValid(false)
    , ResetRegPort(0)
    , ResetVal(0)
    , CenturyRegister(0)
    , HpetBasePhys(0)
    , HpetMinTick(0)
    , FirmwareWatchdog(false)
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

bool Acpi::ParseRsdp(RSDPDescriptor20 *rsdp, ulong& rootPhysAddr, bool& isXsdt)
{
    if (ComputeSum(rsdp, sizeof(rsdp->FirstPart)) != 0)
    {
        Trace(0, "Rsdp 0x%p checksum failed (sum 0x%p)",
            rsdp, (ulong)ComputeSum(rsdp, sizeof(rsdp->FirstPart)));
        return false;
    }

    /* ACPI 2.0+: the extended checksum covers the whole descriptor */
    if (rsdp->FirstPart.Revision >= 2 &&
        ComputeSum(rsdp, sizeof(*rsdp)) != 0)
    {
        Trace(0, "Rsdp 0x%p extended checksum failed (sum 0x%p)",
            rsdp, (ulong)ComputeSum(rsdp, sizeof(*rsdp)));
        return false;
    }

    Stdlib::MemCpy(OemId, rsdp->FirstPart.OEMID, sizeof(rsdp->FirstPart.OEMID));
    OemId[sizeof(rsdp->FirstPart.OEMID)] = '\0';

    /* ACPI 2.0 deprecated the RSDT: its entries are 32-bit, so it cannot
       name a table above 4 GiB, and firmware is free to leave RsdtAddress
       at zero once it supplies an XSDT. Prefer the XSDT whenever the RSDP
       is new enough to have one. The EX44's AMI firmware happens to supply
       both -- Linux uses the XSDT there and this kernel used to use the
       RSDT, which worked only because that firmware is generous. */
    if (rsdp->FirstPart.Revision >= 2 && rsdp->XsdtAddress != 0)
    {
        rootPhysAddr = (ulong)rsdp->XsdtAddress;
        isXsdt = true;
    }
    else
    {
        rootPhysAddr = rsdp->FirstPart.RsdtAddress;
        isXsdt = false;
    }

    Trace(0, "Rsdp 0x%p revision %u OemId %s %s 0x%p",
        rsdp, (ulong)rsdp->FirstPart.Revision, OemId,
        isXsdt ? "Xsdt" : "Rsdt", rootPhysAddr);

    return rootPhysAddr != 0;
}

/* Scan a physical range (16-byte aligned slots) for the RSDP signature.
   Only used for the legacy BIOS areas (EBDA, 0xE0000-0xFFFFF); reading
   arbitrary reserved regions is unsafe on real hardware (MMIO). */
bool Acpi::ScanRsdpRange(ulong phyStart, ulong phyEnd, ulong& rootPhysAddr,
    bool& isXsdt)
{
    auto& pt = Kernel::Mm::PageTable::GetInstance();

    ulong pageStart = Stdlib::RoundDown(phyStart, Const::PageSize);
    for (ulong curr = pageStart; curr < phyEnd; curr += Const::PageSize)
    {
        ulong pageVa = pt.TmpMapPage(curr);
        if (!pageVa)
        {
            Trace(0, "Can't map 0x%p", curr);
            return false;
        }

        ulong scanStart = (curr < phyStart) ? pageVa + (phyStart - curr) : pageVa;
        ulong scanEnd = ((curr + Const::PageSize) > phyEnd)
            ? pageVa + (phyEnd - curr) : pageVa + Const::PageSize;

        for (ulong va = scanStart; va + sizeof(RSDPDescriptor) <= scanEnd; va += 16)
        {
            RSDPDescriptor20 *rsdp = reinterpret_cast<RSDPDescriptor20*>(va);
            if (rsdp->FirstPart.Signature == RSDPSignature)
            {
                Trace(0, "Checking rsdp va 0x%p pha 0x%p", rsdp, curr + (va - pageVa));
                /* ParseRsdp reads the full 36-byte RSDPDescriptor20 when
                   Revision >= 2; the loop bound only guarantees the 20-byte
                   FirstPart, so skip a 2.0 RSDP whose extended fields would
                   read past this single mapped page. */
                if (rsdp->FirstPart.Revision >= 2 &&
                    va + sizeof(RSDPDescriptor20) > scanEnd)
                    continue;
                if (ParseRsdp(rsdp, rootPhysAddr, isXsdt))
                {
                    pt.TmpUnmapPage(pageVa);
                    return true;
                }
            }
        }
        pt.TmpUnmapPage(pageVa);
    }

    return false;
}

bool Acpi::FindRootTable(ulong& rootPhysAddr, bool& isXsdt)
{
    /* 1. RSDP copy from the Multiboot2 ACPI tag. On UEFI this is the
       only way to find it: the RSDP lives in ACPI-reclaimable memory,
       not in the legacy BIOS area. */
    size_t grubRsdpSize = 0;
    const void* grubRsdp = Grub::GetAcpiRsdp(grubRsdpSize);
    if (grubRsdp != nullptr && grubRsdpSize >= sizeof(RSDPDescriptor))
    {
        RSDPDescriptor20 copy;
        Stdlib::MemSet(&copy, 0, sizeof(copy));
        Stdlib::MemCpy(&copy, grubRsdp,
            (grubRsdpSize < sizeof(copy)) ? grubRsdpSize : sizeof(copy));

        if (copy.FirstPart.Signature == RSDPSignature &&
            ParseRsdp(&copy, rootPhysAddr, isXsdt))
        {
            Trace(0, "Rsdp from multiboot tag, %s 0x%p",
                isXsdt ? "Xsdt" : "Rsdt", rootPhysAddr);
            return true;
        }

        Trace(0, "Multiboot rsdp tag invalid");
    }

    auto& pt = Kernel::Mm::PageTable::GetInstance();

    /* 2. First KB of the EBDA; its segment is at BDA 0x40E */
    ulong bdaVa = pt.TmpMapPage(0);
    if (bdaVa != 0)
    {
        ulong ebda = ((ulong)*(u16*)(bdaVa + 0x40E)) << 4;
        pt.TmpUnmapPage(bdaVa);

        if (ebda >= 0x80000 && ebda < 0xA0000)
        {
            if (ScanRsdpRange(ebda, ebda + 1024, rootPhysAddr, isXsdt))
                return true;
        }
    }

    /* 3. BIOS read-only area 0xE0000-0xFFFFF */
    if (ScanRsdpRange(0xE0000, 0x100000, rootPhysAddr, isXsdt))
        return true;

    Trace(0, "Rsdp not found");
    return false;
}

Stdlib::Error Acpi::ParseRootTable(ACPISDTHeader* root)
{
    const char* signature = RootIsXsdt ? "XSDT" : "RSDT";

    if (Stdlib::StrnCmp(root->Signature, signature, sizeof(root->Signature)) != 0)
    {
        Trace(AcpiLL, "%s 0x%p invalid signature", signature, root);
        return MakeError(Stdlib::Error::NotFound);
    }

    if (checkRsdtChecksum)
    {
        if (ComputeSum(root, root->Length) != 0)
        {
            Trace(AcpiLL, "%s 0x%p checksum failed 0x%p vs 0x%p", signature,
                root, (ulong)ComputeSum(root, root->Length), (ulong)root->Checksum);
             return MakeError(Stdlib::Error::NotFound);
        }
    }

    return MakeError(Stdlib::Error::Success);
}

ulong Acpi::RootEntry(size_t index)
{
    const u8* base = reinterpret_cast<const u8*>(Root) +
        OFFSET_OF(ACPISDTHeader, Entry);

    if (RootIsXsdt)
    {
        u64 value;
        Stdlib::MemCpy(&value, base + index * sizeof(u64), sizeof(value));
        return (ulong)value;
    }

    u32 value;
    Stdlib::MemCpy(&value, base + index * sizeof(u32), sizeof(value));
    return (ulong)value;
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

    if (Root->Length <= sizeof(*Root))
        return MakeError(Stdlib::Error::NotFound);

    const size_t entrySize = RootIsXsdt ? sizeof(u64) : sizeof(u32);
    size_t tableCount = (Root->Length - OFFSET_OF(ACPISDTHeader, Entry)) / entrySize;
    Trace(0, "Acpi: %s, %u tables", RootIsXsdt ? "Xsdt" : "Rsdt", tableCount);

    auto& pt = Mm::PageTable::GetInstance();

    for (size_t i = 0; i < tableCount; i++)
    {
        if (i >= Stdlib::ArraySize(Table))
        {
            /* Keep what was collected rather than losing ACPI altogether:
               the tables this kernel looks up are APIC, FACP, HPET, MCFG and
               WDAT, and a machine missing one SSDT still boots, while a
               machine with no ACPI at all panics. */
            Trace(0, "Acpi: table array full at %u of %u, ignoring the rest",
                (ulong)i, (ulong)tableCount);
            break;
        }

        /*
         * First map just the header page to read Length, then re-map
         * the full table so that parsers have a contiguous VA range.
         */
        ulong entryPhys = RootEntry(i);
        ulong physOffset = entryPhys & (Const::PageSize - 1);

        /* TmpMapAddress maps only the entry's page; if the SDT header (whose
           Length field at offset 4 we read next) would straddle the page
           boundary, map the header range instead so the read stays in bounds. */
        bool headerStraddles = (physOffset + sizeof(ACPISDTHeader) > Const::PageSize);
        ACPISDTHeader* header = headerStraddles
            ? reinterpret_cast<ACPISDTHeader*>(pt.TmpMapRange(entryPhys, sizeof(ACPISDTHeader)))
            : reinterpret_cast<ACPISDTHeader*>(pt.TmpMapAddress(entryPhys));
        if (!header)
        {
            Trace(0, "Acpi: can't map table %u phys 0x%p", (ulong)i, entryPhys);
            return MakeError(Stdlib::Error::NoMemory);
        }

        u32 tableLength = header->Length;
        if (tableLength < sizeof(ACPISDTHeader))
        {
            Trace(0, "Acpi: table %u length %u too small", (ulong)i, (ulong)tableLength);
            return MakeError(Stdlib::Error::InvalidValue);
        }

        if (physOffset + tableLength > Const::PageSize)
        {
            /* Table spans pages — unmap the header mapping (one page, or two if
               it straddled) and re-map the full range contiguously. */
            ulong hdrVaPage = reinterpret_cast<ulong>(header) & ~(Const::PageSize - 1);
            pt.TmpUnmapPage(hdrVaPage);
            if (headerStraddles)
                pt.TmpUnmapPage(hdrVaPage + Const::PageSize);
            header = reinterpret_cast<ACPISDTHeader*>(pt.TmpMapRange(entryPhys, tableLength));
            if (!header)
            {
                Trace(0, "Acpi: can't map table %u range phys 0x%p len %u",
                    (ulong)i, entryPhys, (ulong)tableLength);
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
    void* madtEnd = Stdlib::MemAdd(sdtHeader, sdtHeader->Length);

    /* Check the 2-byte entry header is within the table before reading
       entry->Length, then that the whole entry fits -- a truncated or corrupt
       MADT would otherwise read Length past the end of the mapped table. */
    while (Stdlib::MemAdd(entry, sizeof(MadtEntry)) <= madtEnd &&
           Stdlib::MemAdd(entry, entry->Length) <= madtEnd)
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

    /* Century CMOS register selector (ACPI 2.0+, body offset +72). 0 means the
       platform has no century register, so the RTC year must not trust it. */
    static const ulong CenturyEnd = OFFSET_OF(FadtFields, Century) + sizeof(fadt->Century);
    if (bodyLen >= CenturyEnd)
    {
        CenturyRegister = fadt->Century;
        Trace(AcpiLL, "Acpi: FADT century register 0x%p", (ulong)CenturyRegister);
    }
}

u8 Acpi::GetCenturyRegister()
{
    return CenturyRegister;
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

/*
 * WDAT (ACPI "Watchdog Action Table") describes a watchdog the firmware
 * hands to the OS as a list of register instructions.  Its presence means
 * the platform expects the OS to drive the watchdog through those
 * instructions rather than through a native driver -- and it usually
 * describes the very same TCO block the tco_wdt driver would grab.  We do
 * not implement the WDAT instruction interpreter, so all we do here is
 * record the fact and let tco_wdt keep its hands off the hardware.
 */
void Acpi::ParseWDAT()
{
    ACPISDTHeader* sdtHeader = LookupTable("WDAT");
    if (sdtHeader == nullptr)
    {
        Trace(AcpiLL, "Acpi: no WDAT table");
        return;
    }

    if (sdtHeader->Length < sizeof(ACPISDTHeader) + sizeof(WdatTableBody))
    {
        Trace(0, "Acpi: WDAT table too short: %u", (ulong)sdtHeader->Length);
        return;
    }

    WdatTableBody* wdat = reinterpret_cast<WdatTableBody*>(sdtHeader + 1);

    /* Trust the table length over the Entries count */
    size_t maxEntries = (sdtHeader->Length - sizeof(ACPISDTHeader) - sizeof(WdatTableBody))
        / sizeof(WdatEntry);
    size_t entries = wdat->Entries;
    if (entries > maxEntries)
    {
        Trace(0, "Acpi: WDAT claims %u entries, table holds %u",
            (ulong)entries, (ulong)maxEntries);
        entries = maxEntries;
    }

    const WdatEntry* entry = reinterpret_cast<const WdatEntry*>(wdat + 1);
    for (size_t i = 0; i < entries; i++)
    {
        if (entry[i].RegisterRegion.AddressSpaceId == GasSpaceSystemIo &&
            entry[i].RegisterRegion.Address == RtcPortBase)
        {
            Trace(0, "Acpi: WDAT drives the RTC, ignoring it");
            return;
        }
    }

    FirmwareWatchdog = true;

    Trace(0, "Acpi: WDAT present (%u entries, period %u ms), firmware owns the watchdog",
        (ulong)entries, (ulong)wdat->TimerPeriod);
}

Stdlib::Error Acpi::Parse()
{
    Stdlib::Error err;
    ulong rootPhysAddr = 0;
    if (!FindRootTable(rootPhysAddr, RootIsXsdt))
    {
        return MakeError(Stdlib::Error::NotFound);
    }

    auto& pt = Mm::PageTable::GetInstance();

    /* TmpMapAddress maps a single page and SDTs are only 4-byte aligned:
       if the RSDT header (Length at offset 4) straddles the page boundary,
       map the header range instead -- the same defense ParseTablePointers
       applies to every other SDT. */
    ulong rootPhysOff = rootPhysAddr & (Const::PageSize - 1);
    bool headerStraddles = (rootPhysOff + sizeof(ACPISDTHeader) > Const::PageSize);
    ACPISDTHeader* rsdt = headerStraddles
        ? reinterpret_cast<ACPISDTHeader*>(pt.TmpMapRange(rootPhysAddr, sizeof(ACPISDTHeader)))
        : reinterpret_cast<ACPISDTHeader*>(pt.TmpMapAddress(rootPhysAddr));
    if (!rsdt)
        return MakeError(Stdlib::Error::NoMemory);

    u32 rootLength = rsdt->Length;
    if (rootLength < sizeof(ACPISDTHeader))
    {
        Trace(0, "Acpi: rsdt length %u too small", (ulong)rootLength);
        return MakeError(Stdlib::Error::InvalidValue);
    }

    /* Re-map the full table before ParseRootTable: its checksum walks all
       Length bytes, which may extend past the header mapping. */
    if (rootPhysOff + rootLength > Const::PageSize)
    {
        ulong hdrVaPage = reinterpret_cast<ulong>(rsdt) & ~(Const::PageSize - 1);
        pt.TmpUnmapPage(hdrVaPage);
        if (headerStraddles)
            pt.TmpUnmapPage(hdrVaPage + Const::PageSize);
        rsdt = reinterpret_cast<ACPISDTHeader*>(pt.TmpMapRange(rootPhysAddr, rootLength));
        if (!rsdt)
            return MakeError(Stdlib::Error::NoMemory);
    }

    err = ParseRootTable(rsdt);
    if (!err.Ok())
    {
        return err;
    }

    Root = rsdt;

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

    ParseWDAT();

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

bool Acpi::HasFirmwareWatchdog()
{
    return FirmwareWatchdog;
}

}