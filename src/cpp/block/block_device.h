#pragma once

#include <include/types.h>

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

/* What a driver registers: the calls the table makes on it, and what it is.
   A driver written in Rust fills the same table through kcore::block, so
   there is one way into the kernel's device table and not two.

   Everything the ops point at -- the name, the context -- stays the
   driver's, and has to outlive the registration, which is to say the kernel:
   nothing takes a device back. */
struct BlockDeviceOps
{
    /* NUL-terminated, what `disks` shows the device as */
    const char* Name;
    u64 Capacity;                       /* sectors */
    u64 SectorSize;                     /* bytes */

    /* 0 on success, anything else on failure */
    int (*ReadSectors)(void* ctx, u64 sector, void* buf, u32 count);
    int (*WriteSectors)(void* ctx, u64 sector, const void* buf, u32 count, int fua);

    /* nullptr for a device with no write cache to push */
    int (*Flush)(void* ctx);

    /* The asynchronous path: both nullptr for a device without one. Submit
       never blocks and answers with a BlockDevice::Submit* code; Kick is the
       doorbell a submit made without one leaves owed. */
    int (*Submit)(void* ctx, const AsyncBlockIo* io, int kick);
    void (*Kick)(void* ctx);

    void* Ctx;

    /* The disk this is a partition of, as its handle, or 0 for a whole disk.
       Claims are refused through it: one on a disk keeps its partitions out,
       and one on a partition keeps the disk out. */
    ulong Parent;
};

/* A block device the kernel has: a disk, or a partition of one.
 *
 * The device itself lives in the table (src/rust/block), and this is the
 * view of it C++ holds -- a handle and the calls on it. Devices are
 * registered for the life of the kernel, and the table hands out one of
 * these per device, so the pointer is stable and two lookups of the same
 * device compare equal. */
class BlockDevice final
{
public:
    BlockDevice()
        : Handle(0)
    {
    }

    const char* GetName();
    u64 GetCapacity();                  /* sectors */
    u64 GetSectorSize();                /* bytes */
    bool Flush();

    /* The disk a partition is on; nullptr for a whole disk */
    BlockDevice* GetParent();

    bool ReadSectors(u64 sector, void* buf, u32 count);
    bool WriteSectors(u64 sector, const void* buf, u32 count, bool fua = false);

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

    bool CanSubmitAsync();
    int SubmitAsync(const AsyncBlockIo& io, bool kick);
    void KickAsync();

    /* What the table knows this device by, and what the Rust side of the
       layer takes: 0 for a view of nothing. */
    ulong GetHandle() { return Handle; }

    /* Set once interrupts and the scheduler are running.
       Before this, synchronous I/O must poll for completion. */
    static void SetInterruptsStarted();
    static bool GetInterruptsStarted();

private:
    friend class BlockDeviceTable;

    ulong Handle;
};

/* The kernel's block devices. The table itself is in Rust (src/rust/block):
   this is the C++ way in, and holds nothing but a view per device. */
class BlockDeviceTable final
{
public:
    static BlockDeviceTable& GetInstance()
    {
        static BlockDeviceTable instance;
        return instance;
    }

    /* Register a device written in C++. False if the table is full or the
       ops are not a device: no name, no read or write, or a submit without
       the kick its doorbell needs. */
    bool Register(const BlockDeviceOps& ops);

    BlockDevice* Find(const char* name);

    /* The view of the device a handle names, or nullptr */
    BlockDevice* FromHandle(ulong handle);

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

    /* What the table in Rust holds, mirrored here so the views can be an
       array rather than an allocation */
    static const ulong MaxDevices = 48;

private:
    BlockDeviceTable();
    ~BlockDeviceTable();
    BlockDeviceTable(const BlockDeviceTable& other) = delete;
    BlockDeviceTable(BlockDeviceTable&& other) = delete;
    BlockDeviceTable& operator=(const BlockDeviceTable& other) = delete;
    BlockDeviceTable& operator=(BlockDeviceTable&& other) = delete;

    /* One view per slot of the table, handed out by Find and GetDevice */
    BlockDevice Views[MaxDevices];
};

}
