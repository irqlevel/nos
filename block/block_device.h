#pragma once

#include <include/types.h>
#include <lib/printer.h>

namespace Kernel
{

class BlockDevice
{
public:
    virtual ~BlockDevice() {}
    virtual const char* GetName() = 0;
    virtual u64 GetCapacity() = 0;         /* Total sectors */
    virtual u64 GetSectorSize() = 0;       /* Bytes per sector */
    virtual bool ReadSector(u64 sector, void* buf) = 0;
    virtual bool WriteSector(u64 sector, const void* buf) = 0;
};

class BlockDeviceTable
{
public:
    static BlockDeviceTable& GetInstance()
    {
        static BlockDeviceTable instance;
        return instance;
    }

    bool Register(BlockDevice* dev);

    BlockDevice* Find(const char* name);

    void Dump(Stdlib::Printer& printer);

    ulong GetCount();

    BlockDevice* GetDevice(ulong index);

    static const ulong MaxDevices = 48;

private:
    BlockDeviceTable();
    ~BlockDeviceTable();
    BlockDeviceTable(const BlockDeviceTable& other) = delete;
    BlockDeviceTable(BlockDeviceTable&& other) = delete;
    BlockDeviceTable& operator=(const BlockDeviceTable& other) = delete;
    BlockDeviceTable& operator=(BlockDeviceTable&& other) = delete;

    BlockDevice* Devices[MaxDevices];
    ulong Count;
};

}
