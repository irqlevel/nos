#pragma once

#include "stdlib.h"

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

    void* GetRsdp();

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
    } __attribute__ ((packed));

    struct RSDPDescriptor20 {
        RSDPDescriptor FirstPart;
        u32 Length;
        u64 XsdtAddress;
        u8 ExtendedChecksum;
        u8 Reserved[3];
    } __attribute__ ((packed));

    bool ParseRsdp(RSDPDescriptor20 *rsdp);

    static const u64 RSDPSignature = 0x2052545020445352; //'RSD PTR '

};

}

}