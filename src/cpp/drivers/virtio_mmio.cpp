#include "virtio_mmio.h"
#include "virtqueue.h"

#include <mm/mmio.h>
#include <hal/barrier.h>
#include <kernel/trace.h>

namespace Kernel
{

namespace
{

void Write32(ulong addr, u32 value)
{
    Mm::MmIo::Write32((void*)addr, value);
}

u32 Read32(ulong addr)
{
    return Mm::MmIo::Read32((void*)addr);
}

}

VirtioMmio::VirtioMmio()
    : Base(0)
    , CurQueueSize(0)
{
}

VirtioMmio::~VirtioMmio()
{
}

u32 VirtioMmio::ReadDeviceId(ulong base)
{
    if (Read32(base + RegMagic) != MagicValue)
        return 0;
    if (Read32(base + RegVersion) != 2)
        return 0;
    return Read32(base + RegDeviceId);
}

bool VirtioMmio::Probe(ulong base, ulong size)
{
    (void)size;

    if (Read32(base + RegMagic) != MagicValue)
        return false;

    u32 version = Read32(base + RegVersion);
    if (version != 2)
    {
        Trace(0, "VirtioMmio 0x%p: unsupported version %u (need "
            "-global virtio-mmio.force-legacy=false)", base, (ulong)version);
        return false;
    }

    if (Read32(base + RegDeviceId) == 0)
        return false;

    Base = base;
    return true;
}

void VirtioMmio::Reset()
{
    Write32(Base + RegStatus, 0);
}

u8 VirtioMmio::GetStatus()
{
    return (u8)Read32(Base + RegStatus);
}

void VirtioMmio::SetStatus(u8 s)
{
    Write32(Base + RegStatus, s);
}

u32 VirtioMmio::ReadDeviceFeature(ulong index)
{
    Write32(Base + RegDeviceFeaturesSel, (u32)index);
    return Read32(Base + RegDeviceFeatures);
}

void VirtioMmio::WriteDriverFeature(ulong index, u32 val)
{
    Write32(Base + RegDriverFeaturesSel, (u32)index);
    Write32(Base + RegDriverFeatures, val);
}

u16 VirtioMmio::GetNumQueues()
{
    /* No register for this; probe QueueNumMax per queue */
    u16 count = 0;
    for (u16 i = 0; i < 8; i++)
    {
        Write32(Base + RegQueueSel, i);
        if (Read32(Base + RegQueueNumMax) == 0)
            break;
        count = count + 1;
    }
    return count;
}

u8 VirtioMmio::ReadISR()
{
    u32 status = Read32(Base + RegInterruptStatus);
    if (status != 0)
        Write32(Base + RegInterruptAck, status);
    return (u8)status;
}

u8 VirtioMmio::ReadConfigGeneration()
{
    return (u8)Read32(Base + RegConfigGeneration);
}

void VirtioMmio::SelectQueue(u16 idx)
{
    Write32(Base + RegQueueSel, idx);
    CurQueueSize = (u16)Read32(Base + RegQueueNumMax);

    /* mmio reports up to 1024; negotiate down to what VirtQueue supports
       (EnableQueue writes the clamped size into QueueNum) */
    if (CurQueueSize > VirtQueue::MaxDescriptors)
        CurQueueSize = VirtQueue::MaxDescriptors;
}

u16 VirtioMmio::GetQueueSize()
{
    return CurQueueSize;
}

u16 VirtioMmio::GetQueueNotifyOff()
{
    return 0;
}

void VirtioMmio::SetQueueDesc(u64 physAddr)
{
    Write32(Base + RegQueueDescLow, (u32)physAddr);
    Write32(Base + RegQueueDescHigh, (u32)(physAddr >> 32));
}

void VirtioMmio::SetQueueDriver(u64 physAddr)
{
    Write32(Base + RegQueueDriverLow, (u32)physAddr);
    Write32(Base + RegQueueDriverHigh, (u32)(physAddr >> 32));
}

void VirtioMmio::SetQueueDevice(u64 physAddr)
{
    Write32(Base + RegQueueDeviceLow, (u32)physAddr);
    Write32(Base + RegQueueDeviceHigh, (u32)(physAddr >> 32));
}

void VirtioMmio::EnableQueue()
{
    Write32(Base + RegQueueNum, CurQueueSize);
    Hal::DmaWmb();
    Write32(Base + RegQueueReady, 1);
}

volatile void* VirtioMmio::GetNotifyAddr(u16 queueIdx)
{
    (void)queueIdx;
    /* The shared QueueNotify register wants 32-bit writes; drivers fall
       back to NotifyQueue() when there is no per-queue doorbell */
    return nullptr;
}

void VirtioMmio::NotifyQueue(u16 queueIdx)
{
    Hal::DmaWmb();
    Write32(Base + RegQueueNotify, queueIdx);
}

u8 VirtioMmio::ReadDevCfg8(ulong offset)
{
    return *reinterpret_cast<volatile u8*>(Base + RegConfig + offset);
}

u32 VirtioMmio::ReadDevCfg32(ulong offset)
{
    return Read32(Base + RegConfig + offset);
}

u64 VirtioMmio::ReadDevCfg64(ulong offset)
{
    u64 lo = ReadDevCfg32(offset);
    u64 hi = ReadDevCfg32(offset + 4);
    return (hi << 32) | lo;
}

u8 VirtioMmio::EnableMsixVector(u16 index, InterruptHandler& handler)
{
    (void)index;
    (void)handler;
    return 0;
}

}
