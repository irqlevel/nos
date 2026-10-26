#pragma once

#include <lib/printer.h>

class Pci
{
public:
    static Pci& get_instance()
    {
        static Pci instance;
        return instance;
    }
    Pci();
    virtual ~Pci();

    struct DeviceInfo
    {
        u16 bus;
        u16 slot;
        u16 func;
        u16 vendor;
        u16 device;
        u8 base_class;
        u8 sub_class;
        u8 prog_if;
        u8 revision_id;
        u8 interrupt_line;
        u8 interrupt_pin;
        bool valid;
    };

    void dump(stdlib::Printer& printer);
    void scan();

    DeviceInfo* find_device(u16 vendor, u16 device, ulong start_index = 0);
    DeviceInfo* get_device(ulong index);
    ulong get_device_count();

    u8 read_byte(u16 bus, u16 slot, u16 func, u16 offset);
    u16 read_word(u16 bus, u16 slot, u16 func, u16 offset);
    u32 read_dword(u16 bus, u16 slot, u16 func, u16 offset);
    void write_byte(u16 bus, u16 slot, u16 func, u16 offset, u8 value);
    void write_dword(u16 bus, u16 slot, u16 func, u16 offset, u32 value);
    void write_word(u16 bus, u16 slot, u16 func, u16 offset, u16 value);

    /* Walk PCI capability list. Returns config-space offset of capability
       with matching cap_id, or 0 if not found. Pass previous cap's next
       pointer as start_offset to find subsequent caps of the same type. */
    u8 find_capability(u16 bus, u16 slot, u16 func, u8 cap_id, u8 start_offset = 0);

    u32 get_bar(u16 bus, u16 slot, u16 func, u8 bar);
    u8 get_interrupt_line(u16 bus, u16 slot, u16 func);
    u8 get_interrupt_pin(u16 bus, u16 slot, u16 func);
    void enable_bus_mastering(u16 bus, u16 slot, u16 func);

    static const u16 CLS_UNCLASSIFIED = 0x0;
    static const u16 CLS_MASS_STORAGE_CONTROLLER = 0x1;
    static const u16 CLS_NETWORK_CONTROLLER = 0x2;
    static const u16 CLS_DISPLAY_CONTROLLER = 0x3;
    static const u16 CLS_MULTIMEDIA_CONTROLLER = 0x4;
    static const u16 CLS_MEMORY_CONTROLLER = 0x5;
    static const u16 CLS_BRIDGE_DEVICE = 0x6;
    static const u16 CLS_SIMPLE_COMM = 0x7;
    static const u16 CLS_BASE_SYSTEM_PERIPHERAL = 0x8;
    static const u16 CLS_INPUT_DEVICE = 0x9;
    static const u16 CLS_DOCKING_STATION = 0xA;
    static const u16 CLS_PROCESSOR = 0xB;
    static const u16 CLS_SERIAL_BUS = 0xC;
    static const u16 CLS_WIRELESS = 0xD;
    static const u16 CLS_INTELLIGENT_IO = 0xE;
    static const u16 CLS_SATELLITE_COMM = 0xF;
    static const u16 CLS_ENCRYPTION = 0x10;
    static const u16 CLS_SIGNAL_PROCESSING = 0x11;

    static const u16 SUB_CLS_ETHERNET_CONTROLLER = 0x0;
    static const u16 SUB_CLS_VGA_COMPATIBLE_CONTROLLER = 0x0;
    static const u16 SUB_CLS_SCSI_BUS_CONTROLLER = 0x0;
    static const u16 SUB_CLS_IDE_CONTROLLER = 0x1;
    static const u16 SUB_CLS_SATA = 0x6;
    static const u16 SUB_CLS_NV_ME = 0x8;
    static const u16 SUB_CLS_HOST_BRIDGE = 0x0;
    static const u16 SUB_CLS_ISA_BRIDGE = 0x1;
    static const u16 SUB_CLS_PCI_BRIDGE = 0x4;
    static const u16 SUB_CLS_OTHER_BRIDGE = 0x80;
    static const u16 SUB_CLS_FIRE_WIRE = 0x0;
    static const u16 SUB_CLS_USB = 0x3;
    static const u16 SUB_CLS_SM_BUS = 0x5;

    static const u16 VENDOR_INTEL = 0x8086;
    static const u16 VENDOR_VIRTIO = 0x1Af4;
    static const u16 VENDOR_BOCHS = 0x1234;
    static const u16 VENDOR_RED_HAT = 0x1B36;
    static const u16 VENDOR_AMD = 0x1022;
    static const u16 VENDOR_REALTEK = 0x10EC;

    static const u16 DEV_VIRTIO_NETWORK = 0x1000;
    static const u16 DEV_VIRTIO_BLK = 0x1001;
    static const u16 DEV_VIRTIO_CONSOLE = 0x1003;
    static const u16 DEV_VIRTIO_SCSI = 0x1004;
    static const u16 DEV_VIRTIO_RNG = 0x1005;
    static const u16 DEV_VIRTIO_BALLOON = 0x1002;
    static const u16 DEV_VIRTIO_GPU = 0x1050;
    static const u16 DEV_VIRTIO_INPUT = 0x1052;
    static const u16 DEV_VIRTIO_SOCKET = 0x1053;
    static const u16 DEV_VIRTIO_NET_MODERN = 0x1041;
    static const u16 DEV_VIRTIO_BLK_MODERN = 0x1042;
    static const u16 DEV_VIRTIO_RNG_MODERN = 0x1044;
    static const u16 DEV_VIRTIO_SCSI_MODERN = 0x1048;
    static const u16 DEV_RED_HAT_PCIE_BRIDGE = 0x000E;

    const char* class_to_str(u16 cls);

    const char* sub_class_to_str(u16 cls, u16 subcls);

    const char* vendor_to_str(u16 vendor);

    const char* device_to_str(u16 vendor, u16 dev);

private:
    Pci(const Pci& other) = delete;
    Pci(Pci&& other) = delete;
    Pci& operator=(const Pci& other) = delete;
    Pci& operator=(Pci&& other) = delete;

    u16 get_vendor_id(u16 bus, u16 device, u16 function);

    u16 get_device_id(u16 bus, u16 device, u16 function);

    u8 get_class_id(u16 bus, u16 device, u16 function);

    u8 get_sub_class_id(u16 bus, u16 device, u16 function);

    u8 get_prog_if(u16 bus, u16 device, u16 function);

    u8 get_revision_id(u16 bus, u16 device, u16 function);

    static const ulong MAX_DEVICES = 64;
    DeviceInfo devices_[MAX_DEVICES];
    ulong device_count_;
};