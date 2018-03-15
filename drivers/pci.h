#pragma once

#include <lib/printer.h>

class Pci
{
public:
    static Pci& GetInstance()
    {
        static Pci instance;
        return instance;
    }
    Pci();
    virtual ~Pci();

    void Dump(Stdlib::Printer& printer);
    void Scan();
private:
    Pci(const Pci& other) = delete;
    Pci(Pci&& other) = delete;
    Pci& operator=(const Pci& other) = delete;
    Pci& operator=(Pci&& other) = delete;

    u16 ReadWord(u16 bus, u16 slot, u16 func, u16 offset);

    u16 GetVendorID(u16 bus, u16 device, u16 function);

    u16 GetDeviceID(u16 bus, u16 device, u16 function);

    u8 GetClassId(u16 bus, u16 device, u16 function);

    u8 GetSubClassId(u16 bus, u16 device, u16 function);

    u8 GetProgIF(u16 bus, u16 device, u16 function);

    u8 GetRevisionID(u16 bus, u16 device, u16 function);

};