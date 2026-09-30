#pragma once

#include <lib/stdlib.h>
#include <lib/error.h>

namespace Kernel
{

class Acpi final
{
public:
    static Acpi& GetInstance()
    {
        static Acpi Instance;
        return Instance;
    }

    Stdlib::Error Parse();

    void* GetLapicAddress();

    void* GetIoApicAddress();

    u32 GetGsiByIrq(u8 irq);
    u16 GetIrqFlags(u8 irq);

    /* FADT: PM1a control port for ACPI S5 shutdown (0 if not found) */
    ulong GetPm1aCntPort();

    /* FADT: ACPI reset register (I/O port) and value (0/false if not available) */
    bool HasResetReg();
    ulong GetResetRegPort();
    u8 GetResetValue();

    /* FADT: RTC century CMOS register selector (0 if the platform has none) */
    u8 GetCenturyRegister();

    /* HPET: physical base address (0 if no HPET ACPI table) */
    ulong GetHpetBasePhys();
    u16 GetHpetMinTick();

    /* WDAT: true when the firmware describes a watchdog for the OS to drive
       through the ACPI instruction set.  Native watchdog drivers (the Rust
       tco_wdt) must then leave the hardware alone -- the same rule Linux
       applies in acpi_has_watchdog(). */
    bool HasFirmwareWatchdog();

private:
    Acpi();
    ~Acpi();
    Acpi(const Acpi& other) = delete;
    Acpi(Acpi&& other) = delete;
    Acpi& operator=(const Acpi& other) = delete;
    Acpi& operator=(Acpi&& other) = delete;

    struct RSDPDescriptor
    {
        u64 Signature;
        u8 Checksum;
        char OEMID[6];
        u8 Revision;
        u32 RsdtAddress;
    } __attribute__((packed));

    struct RSDPDescriptor20
    {
        RSDPDescriptor FirstPart;
        u32 Length;
        u64 XsdtAddress;
        u8 ExtendedChecksum;
        u8 Reserved[3];
    } __attribute__((packed));

    struct ACPISDTHeader
    {
        char Signature[4];
        u32 Length;
        u8 Revision;
        u8 Checksum;
        char OEMID[6];
        char OEMTableID[8];
        u32 OEMRevision;
        u32 CreatorID;
        u32 CreatorRevision;
        u32 Entry[0];
    } __attribute__((packed));

    static_assert(sizeof(ACPISDTHeader) == 36, "Invalid size");

    struct MadtEntry
    {
        u8 Type;
        u8 Length;
    } __attribute__((packed));

    static const u8 MadtEntryTypeLapic = 0;
    static const u8 MadtEntryTypeIoApic = 1;
    static const u8 MadtEntryTypeIntSrcOverride = 2;

    struct MadtHeader
    {
        u32 LocalIntCtrlAddress;
        u32 Flags;
        MadtEntry Entry[0];
    } __attribute__((packed));

    struct MadtLapicEntry
    {
        u8 AcpiProcessId;
        u8 ApicId;
        u32 Flags;
    } __attribute__((packed));

    struct MadtIoApicEntry
    {
        u8 IoApicId;
        u8 Reserved;
        u32 IoApicAddress;
        u32 GlobalSystemInterruptBase;
    } __attribute__((packed));

    struct MadtIntSrcOverrideEntry
    {
        u8 BusSource;
        u8 IrqSource;
        u32 GlobalSystemInterrupt;
        u16 Flags;
    } __attribute__((packed));

    int ComputeSum(void* table, size_t len);

    /* All three yield the physical address of the root table and which of
       the two kinds it is. See ParseRsdp for why there are two. */
    bool ParseRsdp(RSDPDescriptor20* rsdp, ulong& rootPhysAddr, bool& isXsdt);
    bool FindRootTable(ulong& rootPhysAddr, bool& isXsdt);
    bool ScanRsdpRange(ulong phyStart, ulong phyEnd, ulong& rootPhysAddr,
        bool& isXsdt);
    Stdlib::Error ParseRootTable(ACPISDTHeader* root);

    /* Root entry index -> physical address of that table. The entries are
       32-bit in an RSDT and 64-bit in an XSDT, and in an XSDT they are not
       naturally aligned (the header is 36 bytes), so they are copied out
       rather than dereferenced. */
    ulong RootEntry(size_t index);

    /* Parse's work: what it maps is left for Parse to let go of */
    Stdlib::Error ParseTables();
    Stdlib::Error ParseTablePointers();
    Stdlib::Error ParseMADT();

    /* The table of that signature Parse is reading, and its length */
    ACPISDTHeader* LookupTable(const char *name, u32& length);

    char OemId[7];

    ACPISDTHeader* Root;
    u32 RootLength;
    bool RootIsXsdt;

    /* The tables this kernel reads, one of each kind, mapped through the
       TmpMap window while Parse reads them: Table[i] is WantedTables[i]'s,
       TableLength[i] its length -- what of it is mapped, and all a parser
       reads. Every other table the root lists is looked at and let go of at
       once, and these and the root when Parse is done, whichever way it
       ends: what the kernel keeps of ACPI is the values Parse takes out of
       it, and the local APIC's and the IO-APIC's pages. A firmware's table
       count is not small and not predictable -- the Hetzner EX44 lists 25,
       eleven of them SSDTs, and the count moves with every BIOS revision --
       and a slot of the window held is one every page allocation after it
       goes without. */
    static const size_t MaxTables = 4;
    ACPISDTHeader* Table[MaxTables];
    u32 TableLength[MaxTables];

    /* The longest table mapped, the root or one of those read -- a MADT of
       a thousand CPUs, each with its x2APIC and NMI entries, is 42 KiB. A
       longer one is refused, so that what Parse holds of the window at once
       is bounded: the root and a table of each kind, at most 17 pages
       each. */
    static const u32 MaxTableLength = 64 * 1024;

    /* WantedTables' index of signature, or -1 */
    static long WantedIndex(const char* signature);

    /* The header of the SDT at phys, mapped -- SDTs are only 4-byte
       aligned, so it may straddle two pages -- or nullptr, which is said:
       past the physical address space, or no room in the window */
    ACPISDTHeader* MapHeader(ulong phys);

    /* The SDT at phys, whose header MapHeader mapped, mapped whole, its
       length read once into length; the header's mapping is let go of.
       nullptr, which is said, for a table not to be mapped -- shorter than
       its header, longer than MaxTableLength, reaching past the physical
       address space -- or no room in the window */
    ACPISDTHeader* MapWhole(ACPISDTHeader* header, ulong phys, u32& length);

    /* Let go of the root and the tables */
    void ReleaseTables();

    /* Let go of the TmpMap pages a mapping of len bytes at va holds */
    static void UnmapTableRange(void* va, ulong len);

    static const bool checkRsdtChecksum = false;
    static const u64 RSDPSignature = 0x2052545020445352ULL; //'RSD PTR '

    void* LapicAddress;
    void* IoApicAddress;

    struct IrqToGsiEntry
    {
        u8 Irq;
        u32 Gsi;
        u16 Flags;
    };

    IrqToGsiEntry IrqToGsi[64];
    size_t IrqToGsiSize;

    /* An Interrupt Source Override: the first for an IRQ is the one taken,
       and one naming a GSI past what the interrupt layer takes (an u8) or
       past the table's room is left out, which is said -- none of them is
       reason enough to lose ACPI, and the boot with it. */
    void RegisterIrqToGsi(u8 irq, u32 gsi, u16 flags);

    /* The GSI base of the IO-APIC IoApicAddress maps, ~0U while none */
    u32 IoApicGsiBase;

    /* What of an APIC's page its registers take, from the address the MADT
       gives. The local APIC's are the page, which the architecture puts at
       a page's start (IA32_APIC_BASE holds a page frame); an IO-APIC's are
       words at 0x00 and 0x10, and at 0x40 the EOI register of a later one.
       An address whose registers would run past its page is refused: the
       drivers' accesses would land in the TmpMap window's next slot, and
       in whatever page that maps. */
    static const ulong LapicRegisterBytes = Const::PageSize;
    static const ulong IoApicRegisterBytes = 0x44;

    /* Whether registers of that many bytes at phys stay in its page */
    static bool RegistersInPage(ulong phys, ulong bytes);

    /* Generic Address Structure (ACPI spec 5.2.3.2) */
    struct GenericAddressStructure
    {
        u8  AddressSpaceId;  /* 0 = system memory, 1 = I/O, 2 = PCI config */
        u8  RegisterBitWidth;
        u8  RegisterBitOffset;
        u8  AccessSize;
        u64 Address;
    } __attribute__((packed));

    static_assert(sizeof(GenericAddressStructure) == 12, "Invalid GAS size");

    /* FADT fields we care about (ACPI spec offsets beyond SDT header) */
    struct FadtFields
    {
        /* offset  0 (from after SDT header = absolute offset 36): FirmwareCtrl */
        u32 FirmwareCtrl;       /* +0  */
        u32 Dsdt;               /* +4  */
        u8  Reserved0;          /* +8  */
        u8  PreferredPmProfile; /* +9  */
        u16 SciInt;             /* +10 */
        u32 SmiCmd;             /* +12 */
        u8  AcpiEnable;         /* +16 */
        u8  AcpiDisable;        /* +17 */
        u8  S4BiosReq;          /* +18 */
        u8  PStateCtrl;         /* +19 */
        u32 Pm1aEvtBlk;         /* +20 */
        u32 Pm1bEvtBlk;         /* +24 */
        u32 Pm1aCntBlk;         /* +28 */
        u32 Pm1bCntBlk;         /* +32 */
        u32 Pm2CntBlk;          /* +36 */
        u32 PmTmrBlk;           /* +40 */
        u32 Gpe0Blk;            /* +44 */
        u32 Gpe1Blk;            /* +48 */
        u8  Pm1EvtLen;          /* +52 */
        u8  Pm1CntLen;          /* +53 */
        u8  Pm2CntLen;          /* +54 */
        u8  PmTmrLen;           /* +55 */
        u8  Gpe0BlkLen;         /* +56 */
        u8  Gpe1BlkLen;         /* +57 */
        u8  Gpe1Base;           /* +58 */
        u8  CstCnt;             /* +59 */
        u16 PLvl2Lat;           /* +60 */
        u16 PLvl3Lat;           /* +62 */
        u16 FlushSize;          /* +64 */
        u16 FlushStride;        /* +66 */
        u8  DutyOffset;         /* +68 */
        u8  DutyWidth;          /* +69 */
        u8  DayAlarm;           /* +70 */
        u8  MonAlarm;           /* +71 */
        u8  Century;            /* +72 */
        u16 IaPcBootArch;       /* +73 */
        u8  Reserved1;          /* +75 */
        u32 Flags;              /* +76 */
        GenericAddressStructure ResetReg;   /* +80 */
        u8  ResetValue;         /* +92 */
    } __attribute__((packed));

    /* HPET ACPI table body (beyond SDT header) */
    struct HpetTableBody
    {
        u32 EventTimerBlockId;          /* +0  */
        GenericAddressStructure BaseAddress; /* +4  */
        u8  HpetNumber;                 /* +16 */
        u16 MinimumClockTick;           /* +17 */
        u8  PageProtection;             /* +19 */
    } __attribute__((packed));

    /* WDAT ACPI table body (beyond the SDT header), followed by Entries
       WdatEntry records */
    struct WdatTableBody
    {
        u32 HeaderLength;       /* +0  */
        u16 PciSegment;         /* +4  */
        u8  PciBus;             /* +6  */
        u8  PciDevice;          /* +7  */
        u8  PciFunction;        /* +8  */
        u8  Reserved[3];        /* +9  */
        u32 TimerPeriod;        /* +12, milliseconds per count */
        u32 MaxCount;           /* +16 */
        u32 MinCount;           /* +20 */
        u8  Flags;              /* +24 */
        u8  Reserved2[3];       /* +25 */
        u32 Entries;            /* +28 */
    } __attribute__((packed));

    static_assert(sizeof(WdatTableBody) == 32, "Invalid WDAT body size");

    struct WdatEntry
    {
        u8  Action;             /* +0  */
        u8  Instruction;        /* +1  */
        u16 Reserved;           /* +2  */
        GenericAddressStructure RegisterRegion; /* +4  */
        u32 Value;              /* +16 */
        u32 Mask;               /* +20 */
    } __attribute__((packed));

    static_assert(sizeof(WdatEntry) == 24, "Invalid WDAT entry size");

    /* A WDAT whose instructions poke the RTC is ignored: the CMOS ports are
       not the watchdog's alone (Linux does the same in
       acpi_watchdog_uses_rtc). */
    static const u8 GasSpaceSystemIo = 1;
    static const u64 RtcPortBase = 0x70;

    void ParseFADT();
    void ParseHPET();
    void ParseWDAT();

    /* FADT-derived values */
    ulong Pm1aCntPort;
    bool ResetRegValid;
    ulong ResetRegPort;
    u8 ResetVal;
    u8 CenturyRegister;

    /* HPET-derived values */
    ulong HpetBasePhys;
    u16 HpetMinTick;

    /* WDAT-derived value */
    bool FirmwareWatchdog;

};

}