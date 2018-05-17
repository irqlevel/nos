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

    static const u16 ClsUnclassified = 0x0;
    static const u16 ClsMassStorageController = 0x1;
    static const u16 ClsNetworkController = 0x2;
    static const u16 ClsDisplayController = 0x3;
    static const u16 ClsMultimediaController = 0x4;
    static const u16 ClsMemoryController = 0x5;
    static const u16 ClsBridgeDevice = 0x6;

    static const u16 SubClsEthernetController = 0x0;
    static const u16 SubClsVgaCompatibleController = 0x0;
    static const u16 SubClsSCSIBusController = 0x0;
    static const u16 SubClsIDEController = 0x1;
    static const u16 SubClsHostBridge = 0x0;
    static const u16 SubClsISABridge = 0x1;
    static const u16 SubClsOtherBridge = 0x80;

    static const u16 VendorIntel = 0x8086;
    static const u16 VendorVirtio = 0x1Af4;
    static const u16 VendorBochs = 0x1234;

    static const u16 DevVirtioNetwork = 0x1000;
    static const u16 DevVirtioBlk = 0x1001;
    static const u16 DevVirtioScsi = 0x1004;
    static const u16 DevVirtioRng = 0x1005;

    const char* ClassToStr(u16 cls);

    const char* SubClassToStr(u16 cls, u16 subcls);

    const char* VendorToStr(u16 vendor);

    const char* DeviceToStr(u16 vendor, u16 dev);

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