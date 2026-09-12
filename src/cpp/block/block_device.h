#pragma once

#include <include/types.h>
#include <lib/printer.h>

namespace Kernel
{

/* One asynchronous I/O, straight to or from physical memory: what a caller
   that must not block hands a device. The zero-copy block server (the netblk
   module) has the disk DMA a read into the frame it is about to transmit,
   and a write out of the frame it received. */
struct AsyncBlockIo
{
    enum : u8 { Read = 0, Write = 1, Flush = 2 };

    u8 Op;
    u8 Fua;             /* a write: through the device's cache before Done */
    u16 Reserved;
    u32 Count;          /* sectors; 0 for a flush */
    u64 Sector;
    u64 Phys;           /* the data: physically contiguous, dword aligned */

    /* Called exactly once for an I/O Submit took, from interrupt context --
       no sleeping, no allocating, no freeing: status 0 once the device has
       done it, the device's error otherwise. */
    void (*Done)(void* ctx, int status);
    void* Ctx;
};

static_assert(sizeof(AsyncBlockIo) == 40, "kcore::block::BlockIo mirrors this layout");

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

    /* Asynchronous I/O (AsyncBlockIo), for a device that has it -- NVMe, and
       a partition of an NVMe disk. Never blocks, so task or softirq context
       alike. The io is read before SubmitAsync returns and only its Done and
       Ctx are kept: it may live on the caller's stack, and be submitted again
       as it is after a Busy. With kick false the device may leave its
       doorbell for KickAsync() to ring -- once for a whole batch rather than
       once per I/O, and a doorbell is a write across the bus (under a
       hypervisor, an exit). */
    static const int SubmitOk = 0;
    static const int SubmitBusy = 1;        /* no room now; there will be after a completion */
    static const int SubmitInvalid = 2;     /* out of range, misaligned, or more than it takes at once */
    static const int SubmitUnsupported = 3; /* the synchronous path only */

    virtual bool CanSubmitAsync() { return false; }
    virtual int SubmitAsync(const AsyncBlockIo& io, bool kick) { (void)io; (void)kick; return SubmitUnsupported; }
    virtual void KickAsync() {}

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
