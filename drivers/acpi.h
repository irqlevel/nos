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

};

}