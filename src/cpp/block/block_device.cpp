#include "block_device.h"

#include <lib/stdlib.h>

/* The table itself: src/rust/block/src/table.rs. A device is a handle there,
   and everything below is that handle plus a call. */
extern "C" {

unsigned long kernel_blockdev_register(const Kernel::BlockDeviceOps* ops);
unsigned int kernel_blockdev_count();
unsigned long kernel_blockdev_at(unsigned int index);
unsigned long kernel_blockdev_find(const unsigned char* name, unsigned long nameLen);
const char* kernel_blockdev_name_ptr(unsigned long handle);
unsigned long kernel_blockdev_parent(unsigned long handle);
unsigned long long kernel_blockdev_capacity(unsigned long handle);
unsigned long long kernel_blockdev_sector_size(unsigned long handle);
int kernel_blockdev_read(unsigned long handle, unsigned long long sector,
    void* buf, unsigned int count);
int kernel_blockdev_write(unsigned long handle, unsigned long long sector,
    const void* buf, unsigned int count, int fua);
int kernel_blockdev_flush(unsigned long handle);
int kernel_blockdev_can_submit(unsigned long handle);
int kernel_blockdev_submit(unsigned long handle, const Kernel::AsyncBlockIo* io, int kick);
void kernel_blockdev_kick(unsigned long handle);
unsigned long kernel_blockdev_claim_as(unsigned long handle, const char* holder,
    const char** heldBy);
void kernel_blockdev_release(unsigned long claim);
int kernel_blockdev_interrupts_started();
void kernel_blockdev_set_interrupts_started();

}

namespace Kernel
{

/* The flag itself lives with the rest of the layer, in Rust: the drivers
   that read it are moving there, and the two that have not yet ask through
   these. */
void BlockDevice::SetInterruptsStarted()
{
    kernel_blockdev_set_interrupts_started();
}

bool BlockDevice::GetInterruptsStarted()
{
    return kernel_blockdev_interrupts_started() != 0;
}

const char* BlockDevice::GetName()
{
    const char* name = kernel_blockdev_name_ptr(Handle);
    return name != nullptr ? name : "";
}

u64 BlockDevice::GetCapacity()
{
    return kernel_blockdev_capacity(Handle);
}

u64 BlockDevice::GetSectorSize()
{
    return kernel_blockdev_sector_size(Handle);
}

bool BlockDevice::Flush()
{
    return kernel_blockdev_flush(Handle) == 0;
}

BlockDevice* BlockDevice::GetParent()
{
    return BlockDeviceTable::GetInstance().FromHandle(kernel_blockdev_parent(Handle));
}

bool BlockDevice::ReadSectors(u64 sector, void* buf, u32 count)
{
    return kernel_blockdev_read(Handle, sector, buf, count) == 0;
}

bool BlockDevice::WriteSectors(u64 sector, const void* buf, u32 count, bool fua)
{
    return kernel_blockdev_write(Handle, sector, buf, count, fua ? 1 : 0) == 0;
}

bool BlockDevice::CanSubmitAsync()
{
    return kernel_blockdev_can_submit(Handle) != 0;
}

int BlockDevice::SubmitAsync(const AsyncBlockIo& io, bool kick)
{
    if (io.Done == nullptr)
        return SubmitInvalid;

    return kernel_blockdev_submit(Handle, &io, kick ? 1 : 0);
}

void BlockDevice::KickAsync()
{
    kernel_blockdev_kick(Handle);
}

BlockDeviceTable::BlockDeviceTable()
{
    /* A view per slot, so the handle a lookup answers with is the index into
       this array plus one and the pointer never moves. */
    for (ulong i = 0; i < MaxDevices; i++)
        Views[i].Handle = i + 1;
}

BlockDeviceTable::~BlockDeviceTable()
{
}

bool BlockDeviceTable::Register(const BlockDeviceOps& ops)
{
    return kernel_blockdev_register(&ops) != 0;
}

BlockDevice* BlockDeviceTable::FromHandle(ulong handle)
{
    if (handle == 0 || handle > MaxDevices)
        return nullptr;

    return &Views[handle - 1];
}

BlockDevice* BlockDeviceTable::Find(const char* name)
{
    if (name == nullptr)
        return nullptr;

    return FromHandle(kernel_blockdev_find(
        reinterpret_cast<const unsigned char*>(name), Stdlib::StrLen(name)));
}

ulong BlockDeviceTable::GetCount()
{
    return kernel_blockdev_count();
}

BlockDevice* BlockDeviceTable::GetDevice(ulong index)
{
    if (index >= MaxDevices)
        return nullptr;

    return FromHandle(kernel_blockdev_at((unsigned int)index));
}

ulong BlockDeviceTable::Claim(BlockDevice* dev, const char* holder, const char*& heldBy)
{
    if (dev == nullptr || holder == nullptr)
        return 0;

    const char* held = nullptr;
    ulong claim = kernel_blockdev_claim_as(dev->GetHandle(), holder, &held);
    if (claim == 0)
        heldBy = held;

    return claim;
}

void BlockDeviceTable::Release(ulong claim)
{
    kernel_blockdev_release(claim);
}

}
