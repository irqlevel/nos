#pragma once

#include "stdlib.h"
#include "error.h"

namespace Kernel
{

namespace Core
{

class Acpi final
{
public:
    static Acpi& GetInstance()
    {
        static Acpi instance;
        return instance;
    }

    Shared::Error Parse();

private:
    Acpi();
    ~Acpi();
    Acpi(const Acpi& other) = delete;
    Acpi(Acpi&& other) = delete;
    Acpi& operator=(const Acpi& other) = delete;
    Acpi& operator=(Acpi&& other) = delete;

    struct RSDPDescriptor {
        u64 Signature;
        u8 Checksum;
        char OEMID[6];
        u8 Revision;
        u32 RsdtAddress;
    } __attribute__((packed));

    struct RSDPDescriptor20 {
        RSDPDescriptor FirstPart;
        u32 Length;
        u64 XsdtAddress;
        u8 ExtendedChecksum;
        u8 Reserved[3];
    } __attribute__((packed));

    struct ACPISDTHeader {
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

    int ComputeSum(void* table, size_t len);

    bool ParseRsdp(RSDPDescriptor20* rsdp);
    RSDPDescriptor20* FindRsdp();
    Shared::Error ParseRsdt(ACPISDTHeader* rsdt);

    Shared::Error ParseTablePointers();
    Shared::Error ParseMADT();

    ACPISDTHeader* LookupTable(const char *name);

    char OemId[7];

    RSDPDescriptor20* Rsdp;
    ACPISDTHeader* Rsdt;

    static const size_t MaxTables = 32;
    ACPISDTHeader* Table[MaxTables];

    static const bool checkRsdtChecksum = false;
    static const u64 RSDPSignature = 0x2052545020445352; //'RSD PTR '

};

}

}