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

    struct DeviceInfo
    {
        u16 Bus;
        u16 Slot;
        u16 Func;
        u16 Vendor;
        u16 Device;
        u8 Class;
        u8 SubClass;
        u8 ProgIF;
        u8 RevisionID;
        u8 InterruptLine;
        u8 InterruptPin;
        bool Valid;
    };

    void Dump(Stdlib::Printer& printer);
    void Scan();

    DeviceInfo* FindDevice(u16 vendor, u16 device, ulong startIndex = 0);
    DeviceInfo* GetDevice(ulong index);
    ulong GetDeviceCount();

    u8 ReadByte(u16 bus, u16 slot, u16 func, u16 offset);
    u32 ReadDword(u16 bus, u16 slot, u16 func, u16 offset);
    void WriteByte(u16 bus, u16 slot, u16 func, u16 offset, u8 value);
    void WriteDword(u16 bus, u16 slot, u16 func, u16 offset, u32 value);
    void WriteWord(u16 bus, u16 slot, u16 func, u16 offset, u16 value);

    /* Walk PCI capability list. Returns config-space offset of capability
       with matching capId, or 0 if not found. Pass previous cap's next
       pointer as startOffset to find subsequent caps of the same type. */
    u8 FindCapability(u16 bus, u16 slot, u16 func, u8 capId, u8 startOffset = 0);

    u32 GetBAR(u16 bus, u16 slot, u16 func, u8 bar);
    u8 GetInterruptLine(u16 bus, u16 slot, u16 func);
    u8 GetInterruptPin(u16 bus, u16 slot, u16 func);
    void EnableBusMastering(u16 bus, u16 slot, u16 func);

    static const u16 ClsUnclassified = 0x0;
    static const u16 ClsMassStorageController = 0x1;
    static const u16 ClsNetworkController = 0x2;
    static const u16 ClsDisplayController = 0x3;
    static const u16 ClsMultimediaController = 0x4;
    static const u16 ClsMemoryController = 0x5;
    static const u16 ClsBridgeDevice = 0x6;
    static const u16 ClsSimpleComm = 0x7;
    static const u16 ClsBaseSystemPeripheral = 0x8;
    static const u16 ClsInputDevice = 0x9;
    static const u16 ClsDockingStation = 0xA;
    static const u16 ClsProcessor = 0xB;
    static const u16 ClsSerialBus = 0xC;
    static const u16 ClsWireless = 0xD;
    static const u16 ClsIntelligentIO = 0xE;
    static const u16 ClsSatelliteComm = 0xF;
    static const u16 ClsEncryption = 0x10;
    static const u16 ClsSignalProcessing = 0x11;

    static const u16 SubClsEthernetController = 0x0;
    static const u16 SubClsVgaCompatibleController = 0x0;
    static const u16 SubClsSCSIBusController = 0x0;
    static const u16 SubClsIDEController = 0x1;
    static const u16 SubClsSATA = 0x6;
    static const u16 SubClsNVMe = 0x8;
    static const u16 SubClsHostBridge = 0x0;
    static const u16 SubClsISABridge = 0x1;
    static const u16 SubClsPCIBridge = 0x4;
    static const u16 SubClsOtherBridge = 0x80;
    static const u16 SubClsFireWire = 0x0;
    static const u16 SubClsUSB = 0x3;
    static const u16 SubClsSMBus = 0x5;

    static const u16 VendorIntel = 0x8086;
    static const u16 VendorVirtio = 0x1Af4;
    static const u16 VendorBochs = 0x1234;
    static const u16 VendorRedHat = 0x1B36;
    static const u16 VendorAMD = 0x1022;
    static const u16 VendorRealtek = 0x10EC;

    static const u16 DevVirtioNetwork = 0x1000;
    static const u16 DevVirtioBlk = 0x1001;
    static const u16 DevVirtioConsole = 0x1003;
    static const u16 DevVirtioScsi = 0x1004;
    static const u16 DevVirtioRng = 0x1005;
    static const u16 DevVirtioBalloon = 0x1002;
    static const u16 DevVirtioGpu = 0x1050;
    static const u16 DevVirtioInput = 0x1052;
    static const u16 DevVirtioSocket = 0x1053;
    static const u16 DevVirtioNetModern = 0x1041;
    static const u16 DevVirtioBlkModern = 0x1042;
    static const u16 DevVirtioScsiModern = 0x1048;
    static const u16 DevRedHatPcieBridge = 0x000E;

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

    static const ulong MaxDevices = 64;
    DeviceInfo Devices[MaxDevices];
    ulong DeviceCount;
};