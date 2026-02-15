#pragma once

#include <include/types.h>
#include <kernel/interrupt.h>
#include <block/block_device.h>
#include <block/block_request.h>
#include <kernel/spin_lock.h>
#include <kernel/raw_spin_lock.h>
#include <kernel/atomic.h>
#include <kernel/asm.h>
#include <lib/list_entry.h>
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
    virtual bool Flush() override;
    virtual bool ReadSectors(u64 sector, void* buf, u32 count) override;
    virtual bool WriteSectors(u64 sector, const void* buf, u32 count, bool fua = false) override;

    /* InterruptHandler interface */
    virtual void OnInterruptRegister(u8 irq, u8 vector) override;
    virtual InterruptHandlerFn GetHandlerFn() override;
    virtual void OnInterrupt(Context* ctx) override;

    void Interrupt(Context* ctx);

    /* Submit a block request (caller context). */
    void Submit(BlockRequest* req);

    /* Drain pending requests and submit to hardware (softirq context). */
    void DrainQueue();

    /* Complete finished I/Os (interrupt context). */
    void CompleteIO();

    /* Discover and initialize all virtio-blk devices. */
    static void InitAll();

private:
    VirtioBlk(const VirtioBlk& other) = delete;
    VirtioBlk(VirtioBlk&& other) = delete;
    VirtioBlk& operator=(const VirtioBlk& other) = delete;
    VirtioBlk& operator=(VirtioBlk&& other) = delete;

    /* Virtio-blk request types */
    static const u32 TypeIn    = 0; /* Read */
    static const u32 TypeOut   = 1; /* Write */
    static const u32 TypeFlush = 4; /* Flush */

    /* Feature bits */
    static const u32 FeatureFlush = (1 << 9);

    struct VirtioBlkReq
    {
        u32 Type;
        u32 Reserved;
        u64 Sector;
    } __attribute__((packed));

    static_assert(sizeof(VirtioBlkReq) == 16, "Invalid size");

    static const ulong MaxSlots = 8;

    struct DmaSlot
    {
        VirtioBlkReq* ReqHeader;
        ulong ReqHeaderPhys;
        u8* StatusBuf;
        ulong StatusBufPhys;
        BlockRequest* Request;
        int Head;
    };

    void WaitForCompletion(BlockRequest& req);
    int AllocSlot();
    void FreeSlot(int idx);

    VirtioPci Transport;
    volatile void* QueueNotifyAddr;
    VirtQueue Queue;
    RawSpinLock VirtQueueLock; /* Protects Queue (AddBufs/GetUsed share free chain) */
    u64 CapacitySectors;
    Atomic InterruptCounter;
    int IntVector;

    char DevName[8];
    bool Initialized;
    bool HasFlush;

    /* Request queue (protected by QueueLock) */
    SpinLock QueueLock;
    Stdlib::ListEntry RequestQueue;

    /* DMA slot pool */
    DmaSlot Slots[MaxSlots];
    DmaSlot* SlotByHead[256]; /* descriptor head -> slot lookup */
    Atomic FreeSlotMask;      /* bitmap, bit set = slot free */

    static const ulong MaxInstances = 8;

public:
    static VirtioBlk Instances[MaxInstances];
    static ulong InstanceCount;
};

}
