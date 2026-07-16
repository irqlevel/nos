#include "virtio_blk.h"
#include "virtio_scsi.h"
#include <hal/irqchip.h>
#include <arch/x86_64/ioapic.h>

#include <kernel/trace.h>
#include <hal/cpu.h>
#include <hal/context.h>
#include <hal/irq_stubs.h>
#include <hal/barrier.h>
#include <kernel/panic.h>
#include <kernel/interrupt.h>
#include <arch/x86_64/idt.h>
#include <kernel/softirq.h>
#include <mm/new.h>
#include <mm/page_table.h>
#include <include/const.h>

namespace Kernel
{

VirtioBlk VirtioBlk::Instances[MaxInstances];
ulong VirtioBlk::InstanceCount = 0;

VirtioBlk::VirtioBlk()
    : QueueNotifyAddr(nullptr)
    , CapacitySectors(0)
    , IntVector(-1)
    , Initialized(false)
    , HasFlush(false)
{
    DevName[0] = '\0';
    Stdlib::MemSet(Slots, 0, sizeof(Slots));
    Stdlib::MemSet(SlotByHead, 0, sizeof(SlotByHead));
}

VirtioBlk::~VirtioBlk()
{
}

bool VirtioBlk::Init(Pci::DeviceInfo* pciDev, const char* name)
{
    auto& pci = Pci::GetInstance();

    ulong nameLen = Stdlib::StrLen(name);
    if (nameLen >= sizeof(DevName))
        nameLen = sizeof(DevName) - 1;
    Stdlib::MemCpy(DevName, name, nameLen);
    DevName[nameLen] = '\0';

    /* Enable PCI bus mastering */
    pci.EnableBusMastering(pciDev->Bus, pciDev->Slot, pciDev->Func);

    /* Probe modern virtio-pci capabilities and map MMIO BARs */
    if (!Transport.Probe(pciDev))
    {
        Trace(0, "VirtioBlk %s: Transport.Probe failed", name);
        return false;
    }

    Trace(0, "VirtioBlk %s: %s virtio-pci probed, irq %u",
        name, Transport.IsLegacy() ? "legacy" : "modern",
        (ulong)pciDev->InterruptLine);

    /* Reset device */
    Transport.Reset();

    /* Acknowledge */
    Transport.SetStatus(VirtioPci::StatusAcknowledge);

    /* Driver */
    Transport.SetStatus(VirtioPci::StatusAcknowledge | VirtioPci::StatusDriver);

    /* Read and negotiate features (64-bit via select) */
    u32 devFeatures0 = Transport.ReadDeviceFeature(0);
    Trace(0, "VirtioBlk %s: device features[0] 0x%p", name, (ulong)devFeatures0);

    /* Negotiate FLUSH if device offers it. */
    u32 drvFeatures0 = devFeatures0 & FeatureFlush;
    Transport.WriteDriverFeature(0, drvFeatures0);
    HasFlush = (drvFeatures0 & FeatureFlush) != 0;

    if (!Transport.IsLegacy())
    {
        /* features[1]: set VIRTIO_F_VERSION_1 (bit 32 = index 1 bit 0) */
        u32 devFeatures1 = Transport.ReadDeviceFeature(1);
        u32 drvFeatures1 = devFeatures1 & (1 << 0); /* VIRTIO_F_VERSION_1 */
        Transport.WriteDriverFeature(1, drvFeatures1);
    }

    if (!Transport.IsLegacy())
    {
        /* Set FEATURES_OK (modern only; legacy doesn't have this bit) */
        Transport.SetStatus(VirtioPci::StatusAcknowledge | VirtioPci::StatusDriver |
                            VirtioPci::StatusFeaturesOk);

        /* Verify FEATURES_OK is still set */
        if (!(Transport.GetStatus() & VirtioPci::StatusFeaturesOk))
        {
            Trace(0, "VirtioBlk %s: FEATURES_OK not set by device", name);
            Transport.SetStatus(VirtioPci::StatusFailed);
            return false;
        }
    }

    /* Setup virtqueue 0 (request queue) */
    Transport.SelectQueue(0);
    u16 queueSize = Transport.GetQueueSize();
    Trace(0, "VirtioBlk %s: queue size %u", name, (ulong)queueSize);

    if (queueSize == 0)
    {
        Trace(0, "VirtioBlk %s: queue size is 0", name);
        Transport.SetStatus(VirtioPci::StatusFailed);
        return false;
    }

    if (!Queue.Setup(queueSize))
    {
        Trace(0, "VirtioBlk %s: failed to setup queue", name);
        Transport.SetStatus(VirtioPci::StatusFailed);
        return false;
    }

    if (!Transport.IsLegacy() && Transport.IsMsixEnabled())
    {
        u8 vec = Transport.EnableMsixVector(0, *this);
        if (vec == 0)
            Trace(0, "VirtioBlk %s: MSI-X unavailable, using INTx", name);
    }

    Transport.SetQueueDesc(Queue.GetDescPhys());
    Transport.SetQueueDriver(Queue.GetAvailPhys());
    Transport.SetQueueDevice(Queue.GetUsedPhys());
    Transport.EnableQueue();

    if (!Transport.IsLegacy())
        QueueNotifyAddr = Transport.GetNotifyAddr(0);

    /* Set DRIVER_OK */
    u8 okStatus = VirtioPci::StatusAcknowledge | VirtioPci::StatusDriver |
                  VirtioPci::StatusDriverOk;
    if (!Transport.IsLegacy())
        okStatus |= VirtioPci::StatusFeaturesOk;
    Transport.SetStatus(okStatus);

    /* Read device config: capacity (u64 at offset 0) */
    CapacitySectors = Transport.ReadDevCfg64(0);

    Trace(0, "VirtioBlk %s: capacity %u sectors (%u MB)",
        name, CapacitySectors, (CapacitySectors * 512) / (1024 * 1024));

    /* Allocate 1 DMA page for all slot headers and status bytes.
       Layout: MaxSlots * VirtioBlkReq (16 bytes each) followed by
               MaxSlots * 1-byte status buffers.
       Total: MaxSlots * 17 bytes = 136 bytes, fits in one 4KB page. */
    ulong dmaPhys;
    void* dmaPtr = Mm::AllocMapPages(1, &dmaPhys);
    if (!dmaPtr)
    {
        Trace(0, "VirtioBlk %s: failed to alloc DMA page", name);
        Transport.SetStatus(VirtioPci::StatusFailed);
        return false;
    }

    ulong dmaVirt = (ulong)dmaPtr;
    Stdlib::MemSet(dmaPtr, 0, Const::PageSize);

    for (ulong i = 0; i < MaxSlots; i++)
    {
        Slots[i].ReqHeader = (VirtioBlkReq*)(dmaVirt + i * sizeof(VirtioBlkReq));
        Slots[i].ReqHeaderPhys = dmaPhys + i * sizeof(VirtioBlkReq);
        Slots[i].StatusBuf = (u8*)(dmaVirt + MaxSlots * sizeof(VirtioBlkReq) + i);
        Slots[i].StatusBufPhys = dmaPhys + MaxSlots * sizeof(VirtioBlkReq) + i;
        Slots[i].Request = nullptr;
        Slots[i].Head = -1;
    }

    /* All slots start free */
    FreeSlotMask.Set((1L << MaxSlots) - 1);

    RequestQueue.Init();

    Initialized = true;

    if (!Transport.UsingMsix())
    {
        /* Legacy INTx: vector 0x25 + instance offset. */
        u8 irq = pciDev->InterruptLine;
        u8 vector = 0x25 + (u8)InstanceCount;
        Interrupt::RegisterLevel(*this, irq, vector);
    }

    /* Register as block device */
    BlockDeviceTable::GetInstance().Register(this);

    Trace(0, "VirtioBlk %s: initialized", name);
    return true;
}

const char* VirtioBlk::GetName()
{
    return DevName;
}

u64 VirtioBlk::GetCapacity()
{
    return CapacitySectors;
}

u64 VirtioBlk::GetSectorSize()
{
    return 512;
}

int VirtioBlk::AllocSlot()
{
    for (ulong i = 0; i < MaxSlots; i++)
    {
        if (FreeSlotMask.TestBit(i))
        {
            if (FreeSlotMask.ClearBit(i))
                return (int)i;
        }
    }
    return -1;
}

void VirtioBlk::FreeSlot(int idx)
{
    if (idx >= 0 && (ulong)idx < MaxSlots)
    {
        Slots[idx].Request = nullptr;
        Slots[idx].Head = -1;
        FreeSlotMask.SetBit((ulong)idx);
    }
}

void VirtioBlk::Submit(BlockRequest* req)
{
    {
        Stdlib::AutoLock lock(QueueLock);
        RequestQueue.InsertTail(&req->Link);
    }
    DrainQueue();
}

void VirtioBlk::DrainQueue()
{
    if (!Initialized)
        return;

    /* Dequeue up to MaxSlots requests under the lock */
    BlockRequest* batch[MaxSlots];
    ulong batchCount = 0;

    {
        Stdlib::AutoLock lock(QueueLock);
        while (batchCount < MaxSlots && !RequestQueue.IsEmpty())
        {
            Stdlib::ListEntry* entry = RequestQueue.RemoveHead();
            if (!entry)
                break;
            BlockRequest* req = CONTAINING_RECORD(entry, BlockRequest, Link);
            batch[batchCount] = req;
            batchCount++;
        }
    }

    if (batchCount == 0)
        return;

    auto& pt = Mm::PageTable::GetInstance();
    bool kicked = false;

    for (ulong i = 0; i < batchCount; i++)
    {
        BlockRequest* req = batch[i];

        int slotIdx = AllocSlot();
        if (slotIdx < 0)
        {
            /* No free slots -- put the whole remaining batch back at the
               head in reverse so the original order is preserved (a later
               flush must not leapfrog earlier writes) */
            Stdlib::AutoLock lock(QueueLock);
            for (ulong j = batchCount; j > i; j--)
                RequestQueue.InsertHead(&batch[j - 1]->Link);
            break;
        }

        DmaSlot& slot = Slots[slotIdx];
        slot.Request = req;

        /* Build request header */
        VirtioBlkReq* hdr = slot.ReqHeader;
        if (req->RequestType == BlockRequest::Flush)
        {
            hdr->Type = TypeFlush;
            hdr->Reserved = 0;
            hdr->Sector = 0;
        }
        else
        {
            hdr->Type = (req->RequestType == BlockRequest::Write) ? TypeOut : TypeIn;
            hdr->Reserved = 0;
            hdr->Sector = req->Sector;
        }

        *slot.StatusBuf = 0xFF;

        Hal::DmaWmb();

        int head = -1;

        if (req->RequestType == BlockRequest::Flush)
        {
            /* Flush: 2-descriptor chain (header + status, no data) */
            VirtQueue::BufDesc bufs[2];
            bufs[0].Addr = slot.ReqHeaderPhys;
            bufs[0].Len = sizeof(VirtioBlkReq);
            bufs[0].Writable = false;

            bufs[1].Addr = slot.StatusBufPhys;
            bufs[1].Len = 1;
            bufs[1].Writable = true;

            ulong flags = VirtQueueLock.LockIrqSave();
            head = Queue.AddBufs(bufs, 2);
            /* Publish the slot mapping in the same critical section as
               AddBufs: once the lock drops, a completion on another CPU may
               already reference this head. */
            if (head >= 0 && (ulong)head < sizeof(SlotByHead) / sizeof(SlotByHead[0]))
            {
                slot.Head = head;
                SlotByHead[head] = &slot;
            }
            VirtQueueLock.UnlockIrqRestore(flags);
        }
        else
        {
            /* Data I/O: 3-descriptor chain (header, data, status) */
            ulong bufPhys = pt.VirtToPhys((ulong)req->Buffer);
            if (bufPhys == 0)
            {
                Trace(0, "VirtioBlk %s: VirtToPhys failed for buf 0x%p", DevName, (ulong)req->Buffer);
                req->Success = false;
                req->Completion.Done();
                FreeSlot(slotIdx);
                continue;
            }

            u32 dataLen = req->SectorCount * 512;

            VirtQueue::BufDesc bufs[3];
            bufs[0].Addr = slot.ReqHeaderPhys;
            bufs[0].Len = sizeof(VirtioBlkReq);
            bufs[0].Writable = false;

            bufs[1].Addr = bufPhys;
            bufs[1].Len = dataLen;
            bufs[1].Writable = (req->RequestType == BlockRequest::Read);

            bufs[2].Addr = slot.StatusBufPhys;
            bufs[2].Len = 1;
            bufs[2].Writable = true;

            ulong flags = VirtQueueLock.LockIrqSave();
            head = Queue.AddBufs(bufs, 3);
            if (head >= 0 && (ulong)head < sizeof(SlotByHead) / sizeof(SlotByHead[0]))
            {
                slot.Head = head;
                SlotByHead[head] = &slot;
            }
            VirtQueueLock.UnlockIrqRestore(flags);
        }

        if (head < 0)
        {
            /* Ring full -- return the remaining batch to the queue head
               in reverse to preserve the original order */
            FreeSlot(slotIdx);
            Stdlib::AutoLock lock(QueueLock);
            for (ulong j = batchCount; j > i; j--)
                RequestQueue.InsertHead(&batch[j - 1]->Link);
            break;
        }

        if ((ulong)head >= sizeof(SlotByHead) / sizeof(SlotByHead[0]))
        {
            Trace(0, "VirtioBlk %s: head %u out of range", DevName, (ulong)head);
            req->Success = false;
            req->Completion.Done();
            FreeSlot(slotIdx);
            continue;
        }

        kicked = true;
    }

    if (kicked)
        Transport.NotifyQueue(0);
}

void VirtioBlk::CompleteIO()
{
    bool completed = false;

    for (;;)
    {
        u32 usedId, usedLen;
        DmaSlot* slot = nullptr;

        /* Claim the slot mapping in the same critical section as GetUsed:
           the freed descriptors may be reused (and SlotByHead[usedId]
           rewritten for a new request) by DrainQueue on another CPU the
           moment the lock drops. */
        ulong flags = VirtQueueLock.LockIrqSave();
        bool got = Queue.GetUsed(usedId, usedLen);
        if (got && usedId < sizeof(SlotByHead) / sizeof(SlotByHead[0]))
        {
            slot = SlotByHead[usedId];
            SlotByHead[usedId] = nullptr;
        }
        VirtQueueLock.UnlockIrqRestore(flags);

        if (!got)
            break;

        if (usedId >= sizeof(SlotByHead) / sizeof(SlotByHead[0]))
        {
            Trace(0, "VirtioBlk %s: bad used id %u", DevName, (ulong)usedId);
            continue;
        }

        if (!slot || !slot->Request)
        {
            Trace(0, "VirtioBlk %s: no slot for used id %u", DevName, (ulong)usedId);
            continue;
        }

        BlockRequest* req = slot->Request;
        req->Success = (*slot->StatusBuf == 0);

        int slotIdx = (int)(slot - Slots);
        FreeSlot(slotIdx);

        req->Completion.Done();
        completed = true;
    }

    /* If we freed slots, there may be pending requests to drain */
    if (completed)
        SoftIrq::GetInstance().Raise(SoftIrq::TypeBlkIo);
}

void VirtioBlk::WaitForCompletion(BlockRequest& req)
{
    if (GetInterruptsStarted())
    {
        req.Completion.Wait();
        return;
    }

    /* Early boot: interrupts not yet enabled, poll for completion */
    while (req.Completion.GetCounter() != 0)
    {
        CompleteIO();
        Pause(100);
    }
}

bool VirtioBlk::ReadSectors(u64 sector, void* buf, u32 count)
{
    if (count == 0)
        return true;
    if ((u64)count * 512 > Const::PageSize)
        return false;
    if ((ulong)buf & (Const::PageSize - 1))
    {
        Trace(0, "VirtioBlk %s: ReadSectors buf 0x%p not page-aligned", DevName, (ulong)buf);
        return false;
    }

    BlockRequest req;
    req.RequestType = BlockRequest::Read;
    req.Sector = sector;
    req.SectorCount = count;
    req.Buffer = buf;

    Submit(&req);
    WaitForCompletion(req);

    return req.Success;
}

bool VirtioBlk::WriteSectors(u64 sector, const void* buf, u32 count, bool fua)
{
    if (count == 0)
        return true;
    if ((u64)count * 512 > Const::PageSize)
        return false;
    if ((ulong)buf & (Const::PageSize - 1))
    {
        Trace(0, "VirtioBlk %s: WriteSectors buf 0x%p not page-aligned", DevName, (ulong)buf);
        return false;
    }

    BlockRequest req;
    req.RequestType = BlockRequest::Write;
    req.Sector = sector;
    req.SectorCount = count;
    req.Buffer = (void*)buf;

    Submit(&req);
    WaitForCompletion(req);

    if (!req.Success)
        return false;
    if (fua)
        return Flush();
    return true;
}

bool VirtioBlk::Flush()
{
    if (!HasFlush)
        return true;  /* no write cache, nothing to flush */

    BlockRequest req;
    req.RequestType = BlockRequest::Flush;

    Submit(&req);
    WaitForCompletion(req);

    return req.Success;
}

void VirtioBlk::OnInterruptRegister(u8 irq, u8 vector)
{
    (void)irq;
    IntVector = vector;
    Trace(0, "VirtioBlk %s: interrupt registered vector 0x%p", DevName, (ulong)vector);
}

InterruptHandlerFn VirtioBlk::GetHandlerFn()
{
    return VirtioBlkInterruptStub;
}

void VirtioBlk::OnInterrupt(Context* ctx)
{
    /* Called by shared interrupt dispatch (no EOI here). */
    Interrupt(ctx);
}

void VirtioBlk::Interrupt(Context* ctx)
{
    (void)ctx;

    /* Read ISR status to acknowledge the interrupt on the INTx path only.
       Under MSI-X the ISR status register is not the notification mechanism
       (virtio 1.x 4.1.4.5) and a spec-conforming device leaves it 0, so
       gating on it there would drop every completion. */
    if (!Transport.UsingMsix())
    {
        u8 isr = Transport.ReadISR();
        if (isr == 0)
            return;
    }

    InterruptCounter.Inc();
    InterruptStats::Inc(IrqVirtioBlk);

    /* Complete finished I/Os */
    CompleteIO();
}

/* --- SoftIrq handler for block I/O --- */

static void BlkIoSoftIrqHandler(void* ctx)
{
    (void)ctx;

    for (ulong i = 0; i < VirtioBlk::InstanceCount; i++)
        VirtioBlk::Instances[i].DrainQueue();

    VirtioScsi::DrainAllQueues();
}

void VirtioBlk::InitAll()
{
    auto& pci = Pci::GetInstance();
    InstanceCount = 0;

    /* Global constructors are not called in this kernel (no .init_array
       in the linker script), so BSS-resident objects lack vtable setup.
       Explicitly construct each instance via placement new. */
    for (ulong i = 0; i < MaxInstances; i++)
        new (&Instances[i]) VirtioBlk();

    for (ulong i = 0; i < pci.GetDeviceCount() && InstanceCount < MaxInstances; i++)
    {
        Pci::DeviceInfo* dev = pci.GetDevice(i);
        if (!dev)
            break;

        if (dev->Vendor != Pci::VendorVirtio)
            continue;
        if (dev->Device != Pci::DevVirtioBlk && dev->Device != Pci::DevVirtioBlkModern)
            continue;

        char name[8];
        name[0] = 'v';
        name[1] = 'd';
        name[2] = (char)('a' + InstanceCount);
        name[3] = '\0';

        VirtioBlk& inst = Instances[InstanceCount];
        if (inst.Init(dev, name))
        {
            InstanceCount++;
        }
    }

    /* Register the block I/O softirq handler (shared by VirtioBlk and VirtioScsi). */
    SoftIrq::GetInstance().Register(SoftIrq::TypeBlkIo, BlkIoSoftIrqHandler, nullptr);

    Trace(0, "VirtioBlk: initialized %u devices", InstanceCount);
}

/* Global interrupt handler called from assembly stub. */
extern "C" void VirtioBlkInterrupt(Context* ctx)
{
    /* Route to the first instance that has an active interrupt.
       For simplicity, just notify all instances. */
    for (ulong i = 0; i < VirtioBlk::InstanceCount; i++)
    {
        VirtioBlk::Instances[i].Interrupt(ctx);
    }

    Hal::IrqEoi();
}

}
