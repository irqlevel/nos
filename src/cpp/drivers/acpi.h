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

    /* HPET: physical base address (0 if no HPET ACPI table) */
    ulong GetHpetBasePhys();
    u16 GetHpetMinTick();

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

    bool ParseRsdp(RSDPDescriptor20* rsdp);
    RSDPDescriptor20* FindRsdp();
    Stdlib::Error ParseRsdt(ACPISDTHeader* rsdt);

    Stdlib::Error ParseTablePointers();
    Stdlib::Error ParseMADT();

    ACPISDTHeader* LookupTable(const char *name);

    char OemId[7];

    RSDPDescriptor20* Rsdp;
    ACPISDTHeader* Rsdt;

    static const size_t MaxTables = 32;
    ACPISDTHeader* Table[MaxTables];

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

    bool RegisterIrqToGsi(u8 irq, u32 gsi, u16 flags);

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

    void ParseFADT();
    void ParseHPET();

    /* FADT-derived values */
    ulong Pm1aCntPort;
    bool ResetRegValid;
    ulong ResetRegPort;
    u8 ResetVal;

    /* HPET-derived values */
    ulong HpetBasePhys;
    u16 HpetMinTick;

};

}