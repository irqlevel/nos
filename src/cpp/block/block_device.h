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
    virtual bool Flush() { return true; }
    /* The disk a partition is on; nullptr for a whole disk */
    virtual BlockDevice* GetParent() { return nullptr; }
    virtual bool ReadSectors(u64 sector, void* buf, u32 count) = 0;
    virtual bool WriteSectors(u64 sector, const void* buf, u32 count, bool fua = false) = 0;

    /* Set once interrupts and the scheduler are running.
       Before this, synchronous I/O must poll for completion. */
    static void SetInterruptsStarted();
    static bool GetInterruptsStarted();

private:
    static bool InterruptsStarted;
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

    /* Exclusive users of a device: a mounted filesystem, the disk log, code
       writing to it around both (a module's, through kcore::block). A claim
       is refused while another overlaps it -- the same device, the disk a
       partition is on, or a partition of that disk -- and heldBy then names
       the holder. holder has to outlive the claim. Reads need no claim.
       Returns a handle for Release, 0 when refused. */
    ulong Claim(BlockDevice* dev, const char* holder, const char*& heldBy);
    void Release(ulong claim);

    /* Whether writing to one can touch the other: the same device, or a disk
       and a partition of it */
    static bool Overlap(BlockDevice* a, BlockDevice* b);

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
