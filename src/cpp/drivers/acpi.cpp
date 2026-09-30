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
    , RootLength(0)
    , RootIsXsdt(false)
    , LapicAddress(nullptr)
    , IoApicAddress(nullptr)
    , IrqToGsiSize(0)
    , IoApicGsiBase(~0U)
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
        TableLength[i] = 0;
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
        Trace(0, "Rsdp 0x%p checksum failed (sum 0x%lX)",
            rsdp, (ulong)ComputeSum(rsdp, sizeof(rsdp->FirstPart)));
        return false;
    }

    /* ACPI 2.0+: the extended checksum covers the whole descriptor */
    if (rsdp->FirstPart.Revision >= 2 &&
        ComputeSum(rsdp, sizeof(*rsdp)) != 0)
    {
        Trace(0, "Rsdp 0x%p extended checksum failed (sum 0x%lX)",
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

    Trace(0, "Rsdp 0x%p revision %lu OemId %s %s 0x%lX",
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
            Trace(0, "Can't map 0x%lX", curr);
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
                Trace(0, "Checking rsdp va 0x%p pha 0x%lX", rsdp, curr + (va - pageVa));
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
            Trace(0, "Rsdp from multiboot tag, %s 0x%lX",
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
        if (ComputeSum(root, RootLength) != 0)
        {
            Trace(AcpiLL, "%s 0x%p checksum failed 0x%lX vs 0x%lX", signature,
                root, (ulong)ComputeSum(root, RootLength), (ulong)root->Checksum);
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

namespace
{

/* What this kernel reads of ACPI: the MADT, the FADT, the HPET table and
   the WDAT (Acpi::Table has one slot for each) */
const char* const WantedTables[] = { "APIC", "FACP", "HPET", "WDAT" };

}

long Acpi::WantedIndex(const char* signature)
{
    static_assert(sizeof(WantedTables) / sizeof(WantedTables[0]) == MaxTables, "a slot of Table for each table read");

    for (size_t i = 0; i < Stdlib::ArraySize(WantedTables); i++)
    {
        if (Stdlib::StrnCmp(signature, WantedTables[i], 4) == 0)
            return (long)i;
    }

    return -1;
}

void Acpi::UnmapTableRange(void* va, ulong len)
{
    auto& pt = Mm::PageTable::GetInstance();

    ulong start = reinterpret_cast<ulong>(va) & ~(Const::PageSize - 1);
    ulong end = reinterpret_cast<ulong>(va) + len;
    for (ulong page = start; page < end; page += Const::PageSize)
        pt.TmpUnmapPage(page);
}

Acpi::ACPISDTHeader* Acpi::MapHeader(ulong phys)
{
    if (phys > Mm::MemoryMap::MaxPhysAddr - sizeof(ACPISDTHeader))
    {
        Trace(0, "Acpi: table at 0x%lX, past the physical address space", phys);
        return nullptr;
    }

    auto* header = reinterpret_cast<ACPISDTHeader*>(
        Mm::PageTable::GetInstance().TmpMapRange(phys, sizeof(ACPISDTHeader)));
    if (header == nullptr)
        Trace(0, "Acpi: can't map table at 0x%lX", phys);
    return header;
}

Acpi::ACPISDTHeader* Acpi::MapWhole(ACPISDTHeader* header, ulong phys, u32& length)
{
    length = header->Length;
    char signature[5];
    Stdlib::MemCpy(signature, header->Signature, sizeof(header->Signature));
    signature[4] = '\0';

    if (length < sizeof(ACPISDTHeader) || length > MaxTableLength ||
        length > Mm::MemoryMap::MaxPhysAddr - phys)
    {
        Trace(0, "Acpi: %s at 0x%lX length %lu, not a table to map", signature, phys, (ulong)length);
        UnmapTableRange(header, sizeof(ACPISDTHeader));
        return nullptr;
    }

    /* Mapped again, as long as it says it is: what a parser reads of it is
       that range, and the fuzzer holds the parsers to it byte for byte */
    UnmapTableRange(header, sizeof(ACPISDTHeader));
    auto* table = reinterpret_cast<ACPISDTHeader*>(Mm::PageTable::GetInstance().TmpMapRange(phys, length));
    if (table == nullptr)
        Trace(0, "Acpi: can't map %s at 0x%lX length %lu", signature, phys, (ulong)length);
    return table;
}

void Acpi::ReleaseTables()
{
    for (size_t i = 0; i < Stdlib::ArraySize(Table); i++)
    {
        if (Table[i] != nullptr)
        {
            UnmapTableRange(Table[i], TableLength[i]);
            Table[i] = nullptr;
            TableLength[i] = 0;
        }
    }

    if (Root != nullptr)
    {
        UnmapTableRange(Root, RootLength);
        Root = nullptr;
        RootLength = 0;
    }
}

Acpi::ACPISDTHeader* Acpi::LookupTable(const char *name, u32& length)
{
    length = 0;
    if (Stdlib::StrLen(name) != 4)
    {
        return nullptr;
    }

    long index = WantedIndex(name);
    if (index < 0)
        return nullptr;

    length = TableLength[index];
    return Table[index];
}

Stdlib::Error Acpi::ParseTablePointers()
{
    if (RootLength <= sizeof(ACPISDTHeader))
        return MakeError(Stdlib::Error::NotFound);

    const size_t entrySize = RootIsXsdt ? sizeof(u64) : sizeof(u32);
    size_t tableCount = (RootLength - OFFSET_OF(ACPISDTHeader, Entry)) / entrySize;
    Trace(0, "Acpi: %s, %lu tables", RootIsXsdt ? "Xsdt" : "Rsdt", tableCount);

    for (size_t i = 0; i < tableCount; i++)
    {
        /* The header first, for the signature: a table this kernel reads is
           then mapped whole, so that its parser has it at one VA range */
        ulong entryPhys = RootEntry(i);
        ACPISDTHeader* header = MapHeader(entryPhys);
        if (header == nullptr)
            continue;

        char tableSignature[5];
        Stdlib::MemCpy(tableSignature, header->Signature, sizeof(header->Signature));
        tableSignature[4] = '\0';

        /* A table this kernel does not read, or a second of one it does, is
           let go of here: neither is worth a slot of the window */
        long wanted = WantedIndex(tableSignature);
        if (wanted < 0 || Table[wanted] != nullptr)
        {
            Trace(AcpiLL, "Acpi: table %lu %s len %lu not kept", (ulong)i, tableSignature,
                (ulong)header->Length);
            UnmapTableRange(header, sizeof(ACPISDTHeader));
            continue;
        }

        u32 tableLength = 0;
        ACPISDTHeader* table = MapWhole(header, entryPhys, tableLength);
        if (table == nullptr)
            continue;

        Trace(AcpiLL, "Acpi: table 0x%p %s len %lu", table, tableSignature, (ulong)tableLength);

        Table[wanted] = table;
        TableLength[wanted] = tableLength;
    }

    return MakeError(Stdlib::Error::Success);
}

bool Acpi::RegistersInPage(ulong phys, ulong bytes)
{
    return (phys & (Const::PageSize - 1)) <= Const::PageSize - bytes;
}

Stdlib::Error Acpi::ParseMADT()
{
    u32 length = 0;
    ACPISDTHeader* sdtHeader = LookupTable("APIC", length);
    if (sdtHeader == nullptr)
    {
        return MakeError(Stdlib::Error::NotFound);
    }

    Trace(AcpiLL, "Acpi: MADT 0x%p", sdtHeader);

    if (length < sizeof(ACPISDTHeader) + sizeof(MadtHeader))
    {
        Trace(0, "Acpi: MADT too short: %lu", (ulong)length);
        return MakeError(Stdlib::Error::InvalidValue);
    }

    MadtHeader* header = reinterpret_cast<MadtHeader*>(sdtHeader + 1);
    Trace(AcpiLL, "Acpi: MADT LIntCtrl 0x%lX flags 0x%lX",
        (ulong)header->LocalIntCtrlAddress, (ulong)header->Flags);

    const ulong lapicPhys = header->LocalIntCtrlAddress;
    if (!RegistersInPage(lapicPhys, LapicRegisterBytes))
    {
        Trace(0, "Acpi: MADT local APIC at 0x%lX, not a page", lapicPhys);
        return MakeError(Stdlib::Error::InvalidValue);
    }

    LapicAddress = (void *)Mm::PageTable::GetInstance().TmpMapAddress(lapicPhys);
    if (LapicAddress == nullptr)
    {
        return MakeError(Stdlib::Error::NoMemory);
    }

    MadtEntry* entry = &header->Entry[0];
    void* madtEnd = Stdlib::MemAdd(sdtHeader, length);

    /* Check the 2-byte entry header is within the table before reading
       entry->Length, then that the whole entry fits -- a truncated or corrupt
       MADT would otherwise read Length past the end of the mapped table. */
    while (Stdlib::MemAdd(entry, sizeof(MadtEntry)) <= madtEnd &&
           Stdlib::MemAdd(entry, entry->Length) <= madtEnd)
    {
        Trace(AcpiLL, "Acpi: MADT entry 0x%p type %lu len %lu",
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

            Trace(AcpiLL, "Acpi: MADT lapic procId %lu apicId %lu flags 0x%lX",
                (ulong)lapicEntry->AcpiProcessId, (ulong)lapicEntry->ApicId, (ulong)lapicEntry->Flags);

            if (lapicEntry->Flags & 0x1)
            {
                if (!CpuTable::GetInstance().InsertCpu(lapicEntry->ApicId))
                {
                    Trace(AcpiLL, "Acpi: MADT lapic apicId %lu ignored (max %lu)",
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

            Trace(AcpiLL, "Acpi: MADT ioApicId %lu addr 0x%lX gsi 0x%lX",
                (ulong)ioApicEntry->IoApicId, (ulong)ioApicEntry->IoApicAddress,
                (ulong)ioApicEntry->GlobalSystemInterruptBase);

            /* The IO-APIC driver takes a GSI for the index of its pin, so the
               one it drives is the one whose pins start at GSI 0 -- the
               legacy IRQs' -- or, on a machine with none such, the first.
               A machine may have several (AMD's FCH and GNB): only the one
               kept is mapped. */
            const u32 base = ioApicEntry->GlobalSystemInterruptBase;
            const ulong ioApicPhys = ioApicEntry->IoApicAddress;
            if (!RegistersInPage(ioApicPhys, IoApicRegisterBytes))
            {
                Trace(0, "Acpi: MADT IO-APIC at 0x%lX, its registers across a page, ignored", ioApicPhys);
                break;
            }
            if (IoApicAddress != nullptr && (IoApicGsiBase == 0 || base != 0))
                break;

            void* mapped = (void *)Mm::PageTable::GetInstance().TmpMapAddress(ioApicPhys);
            if (mapped == nullptr)
            {
                return MakeError(Stdlib::Error::NoMemory);
            }

            if (IoApicAddress != nullptr)
                UnmapTableRange(IoApicAddress, 1);
            IoApicAddress = mapped;
            IoApicGsiBase = base;
            break;
        }
        case MadtEntryTypeIntSrcOverride:
        {
            if (entry->Length < sizeof(MadtIntSrcOverrideEntry) + sizeof(*entry))
                return MakeError(Stdlib::Error::InvalidValue);
            MadtIntSrcOverrideEntry* isoEntry = reinterpret_cast<MadtIntSrcOverrideEntry*>(entry + 1);

            Trace(AcpiLL, "Acpi: MADT bus 0x%lX irq 0x%lX gsi 0x%lX flags 0x%lX",
                (ulong)isoEntry->BusSource, (ulong)isoEntry->IrqSource, (ulong)isoEntry->GlobalSystemInterrupt,
                (ulong)isoEntry->Flags);

            RegisterIrqToGsi(isoEntry->IrqSource, isoEntry->GlobalSystemInterrupt, isoEntry->Flags);
            break;
        }
        default:
            break;
        }

        entry = static_cast<MadtEntry*>(Stdlib::MemAdd(entry, entry->Length));
    }

    /* The interrupts are the IO-APIC's to route: without one to drive, the
       boot would fault in its driver at address 0 */
    if (IoApicAddress == nullptr)
    {
        Trace(0, "Acpi: MADT names no IO-APIC");
        return MakeError(Stdlib::Error::NotFound);
    }

    return MakeError(Stdlib::Error::Success);
}

void Acpi::ParseFADT()
{
    u32 length = 0;
    ACPISDTHeader* sdtHeader = LookupTable("FACP", length);
    if (sdtHeader == nullptr)
    {
        Trace(AcpiLL, "Acpi: no FADT table");
        return;
    }

    ulong bodyLen = length - sizeof(ACPISDTHeader);
    FadtFields* fadt = reinterpret_cast<FadtFields*>(sdtHeader + 1);

    /* Pm1aCntBlk sits at body offset +28; need at least 32 bytes of body */
    static const ulong Pm1aCntBlkEnd = OFFSET_OF(FadtFields, Pm1aCntBlk) + sizeof(fadt->Pm1aCntBlk);
    if (bodyLen >= Pm1aCntBlkEnd)
    {
        Pm1aCntPort = fadt->Pm1aCntBlk;
        Trace(AcpiLL, "Acpi: FADT PM1a_CNT port 0x%lX", Pm1aCntPort);
    }

    /* Flags + ResetReg + ResetValue require at least 93 bytes of body (ACPI 2.0+) */
    static const ulong ResetValueEnd = OFFSET_OF(FadtFields, ResetValue) + sizeof(fadt->ResetValue);
    if (bodyLen >= ResetValueEnd)
    {
        Trace(AcpiLL, "Acpi: FADT flags 0x%lX", (ulong)fadt->Flags);

        /* RESET_REG_SUP is bit 10 of Flags */
        static const u32 ResetRegSup = (1u << 10);
        if ((fadt->Flags & ResetRegSup) && fadt->ResetReg.AddressSpaceId == 1 /* I/O */)
        {
            ResetRegValid = true;
            ResetRegPort = (ulong)fadt->ResetReg.Address;
            ResetVal = fadt->ResetValue;
            Trace(AcpiLL, "Acpi: FADT RESET_REG port 0x%lX value 0x%lX",
                ResetRegPort, (ulong)ResetVal);
        }
    }

    /* Century CMOS register selector (ACPI 2.0+, body offset +72). 0 means the
       platform has no century register, so the RTC year must not trust it. */
    static const ulong CenturyEnd = OFFSET_OF(FadtFields, Century) + sizeof(fadt->Century);
    if (bodyLen >= CenturyEnd)
    {
        CenturyRegister = fadt->Century;
        Trace(AcpiLL, "Acpi: FADT century register 0x%lX", (ulong)CenturyRegister);
    }
}

u8 Acpi::GetCenturyRegister()
{
    return CenturyRegister;
}

void Acpi::ParseHPET()
{
    u32 length = 0;
    ACPISDTHeader* sdtHeader = LookupTable("HPET", length);
    if (sdtHeader == nullptr)
    {
        Trace(AcpiLL, "Acpi: no HPET table");
        return;
    }

    if (length < sizeof(ACPISDTHeader) + sizeof(HpetTableBody))
    {
        Trace(0, "Acpi: HPET table too short: %lu", (ulong)length);
        return;
    }

    HpetTableBody* hpet = reinterpret_cast<HpetTableBody*>(sdtHeader + 1);

    /* BaseAddress must be system memory (AddressSpaceId == 0) */
    if (hpet->BaseAddress.AddressSpaceId != 0)
    {
        Trace(0, "Acpi: HPET base not in system memory (id %lu)", (ulong)hpet->BaseAddress.AddressSpaceId);
        return;
    }

    HpetBasePhys = (ulong)hpet->BaseAddress.Address;
    HpetMinTick  = hpet->MinimumClockTick;

    Trace(AcpiLL, "Acpi: HPET base 0x%lX minTick %lu blockId 0x%lX",
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
    u32 length = 0;
    ACPISDTHeader* sdtHeader = LookupTable("WDAT", length);
    if (sdtHeader == nullptr)
    {
        Trace(AcpiLL, "Acpi: no WDAT table");
        return;
    }

    if (length < sizeof(ACPISDTHeader) + sizeof(WdatTableBody))
    {
        Trace(0, "Acpi: WDAT table too short: %lu", (ulong)length);
        return;
    }

    WdatTableBody* wdat = reinterpret_cast<WdatTableBody*>(sdtHeader + 1);

    /* Trust the table length over the Entries count */
    size_t maxEntries = (length - sizeof(ACPISDTHeader) - sizeof(WdatTableBody))
        / sizeof(WdatEntry);
    size_t entries = wdat->Entries;
    if (entries > maxEntries)
    {
        Trace(0, "Acpi: WDAT claims %lu entries, table holds %lu",
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

    Trace(0, "Acpi: WDAT present (%lu entries, period %lu ms), firmware owns the watchdog",
        (ulong)entries, (ulong)wdat->TimerPeriod);
}

Stdlib::Error Acpi::Parse()
{
    Stdlib::Error err = ParseTables();

    /* The tables are read once, here: what is kept of them is the values
       taken out, and on a failure not even the APICs' pages, which nothing
       is then to use */
    ReleaseTables();
    if (!err.Ok())
    {
        if (LapicAddress != nullptr)
        {
            UnmapTableRange(LapicAddress, 1);
            LapicAddress = nullptr;
        }
        if (IoApicAddress != nullptr)
        {
            UnmapTableRange(IoApicAddress, 1);
            IoApicAddress = nullptr;
            IoApicGsiBase = ~0U;
        }
    }

    return err;
}

Stdlib::Error Acpi::ParseTables()
{
    Stdlib::Error err;
    ulong rootPhysAddr = 0;
    if (!FindRootTable(rootPhysAddr, RootIsXsdt))
    {
        return MakeError(Stdlib::Error::NotFound);
    }

    ACPISDTHeader* header = MapHeader(rootPhysAddr);
    if (header == nullptr)
        return MakeError(Stdlib::Error::NoMemory);

    Root = MapWhole(header, rootPhysAddr, RootLength);
    if (Root == nullptr)
        return MakeError(Stdlib::Error::InvalidValue);

    err = ParseRootTable(Root);
    if (!err.Ok())
    {
        return err;
    }

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

void Acpi::RegisterIrqToGsi(u8 irq, u32 gsi, u16 flags)
{
    /* Interrupt::Register takes a GSI in an u8 */
    static const u32 MaxGsi = 0xFF;

    for (size_t i = 0; i < IrqToGsiSize; i++)
    {
        if (IrqToGsi[i].Irq == irq)
        {
            Trace(0, "Acpi: second override of irq %lu (to gsi %lu) ignored", (ulong)irq, (ulong)gsi);
            return;
        }
    }

    if (gsi > MaxGsi || IrqToGsiSize >= Stdlib::ArraySize(IrqToGsi))
    {
        Trace(0, "Acpi: override of irq %lu to gsi %lu ignored: %s", (ulong)irq, (ulong)gsi,
            (gsi > MaxGsi) ? "past the GSIs taken" : "no room");
        return;
    }

    auto& entry = IrqToGsi[IrqToGsiSize];
    entry.Irq = irq;
    entry.Gsi = gsi;
    entry.Flags = flags;
    IrqToGsiSize++;
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