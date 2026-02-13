#pragma once

#include <include/types.h>
#include <kernel/interrupt.h>
#include <block/block_device.h>
#include <kernel/spin_lock.h>
#include <kernel/atomic.h>
#include <kernel/asm.h>
#include <drivers/virtqueue.h>
#include <drivers/pci.h>
#include <drivers/virtio_pci.h>

namespace Kernel
{

class VirtioBlk : public BlockDevice, public InterruptHandler
{
public:
    VirtioBlk();
    virtual ~VirtioBlk();

    bool Init(Pci::DeviceInfo* pciDev, const char* name);

    /* BlockDevice interface */
    virtual const char* GetName() override;
    virtual u64 GetCapacity() override;
    virtual u64 GetSectorSize() override;
    virtual bool ReadSector(u64 sector, void* buf) override;
    virtual bool WriteSector(u64 sector, const void* buf) override;

    /* InterruptHandler interface */
    virtual void OnInterruptRegister(u8 irq, u8 vector) override;
    virtual InterruptHandlerFn GetHandlerFn() override;
    virtual void OnInterrupt(Context* ctx) override;

    void Interrupt(Context* ctx);

    /* Discover and initialize all virtio-blk devices. */
    static void InitAll();

private:
    VirtioBlk(const VirtioBlk& other) = delete;
    VirtioBlk(VirtioBlk&& other) = delete;
    VirtioBlk& operator=(const VirtioBlk& other) = delete;
    VirtioBlk& operator=(VirtioBlk&& other) = delete;

    bool DoIO(u32 type, u64 sector, void* buf);

    /* Virtio-blk request types */
    static const u32 TypeIn  = 0; /* Read */
    static const u32 TypeOut = 1; /* Write */

    struct VirtioBlkReq
    {
        u32 Type;
        u32 Reserved;
        u64 Sector;
    } __attribute__((packed));

    static_assert(sizeof(VirtioBlkReq) == 16, "Invalid size");

    VirtioPci Transport;
    volatile void* QueueNotifyAddr;
    VirtQueue Queue;
    u64 CapacitySectors;
    SpinLock IoLock;
    Atomic InterruptCounter;
    int IntVector;

    char DevName[8];
    bool Initialized;

    /* DMA buffers (identity-mapped) */
    VirtioBlkReq* ReqHeader;
    ulong ReqHeaderPhys;
    u8* DataBuf;
    ulong DataBufPhys;
    u8* StatusBuf;
    ulong StatusBufPhys;

    static const ulong MaxInstances = 8;

public:
    static VirtioBlk Instances[MaxInstances];
    static ulong InstanceCount;
};

}