#include "virtio_scsi.h"
#include "lapic.h"
#include "ioapic.h"

#include <kernel/trace.h>
#include <kernel/asm.h>
#include <kernel/panic.h>
#include <kernel/interrupt.h>
#include <kernel/idt.h>
#include <kernel/softirq.h>
#include <mm/new.h>
#include <mm/page_table.h>
#include <include/const.h>

namespace Kernel
{

VirtioScsi VirtioScsi::Instances[MaxInstances];
ulong VirtioScsi::InstanceCount = 0;

VirtioScsi::HbaState VirtioScsi::Hbas[MaxHbas];
ulong VirtioScsi::HbaCount = 0;

VirtioScsi::VirtioScsi()
    : Transport(nullptr)
    , ReqQueue(nullptr)
    , IoLock(nullptr)
    , Target(0)
    , LunId(0)
    , CapacitySectors(0)
    , SectorSz(DefaultSectorSize)
    , IntVector(-1)
    , Initialized(false)
    , ReqHdrSize(0)
    , RespHdrSize(0)
    , CmdReq(nullptr)
    , CmdReqPhys(0)
    , CmdResp(nullptr)
    , CmdRespPhys(0)
    , DataBuf(nullptr)
    , DataBufPhys(0)
    , Hba(nullptr)
{
    DevName[0] = '\0';
}

VirtioScsi::~VirtioScsi()
{
}

void VirtioScsi::EncodeLun(u8 lun[8], u8 target, u16 lunId)
{
    lun[0] = 0x01;
    lun[1] = target;
    lun[2] = (u8)((lunId >> 8) | 0x40); /* SAM-2 single-level LUN */
    lun[3] = (u8)(lunId & 0xFF);
    lun[4] = 0;
    lun[5] = 0;
    lun[6] = 0;
    lun[7] = 0;
}

bool VirtioScsi::Init(VirtioPci* transport, VirtQueue* reqQueue, SpinLock* ioLock,
                       u8 target, u16 lun, u64 capacity, u64 sectorSize, const char* name,
                       u32 reqHdrSize, u32 respHdrSize)
{
    Transport = transport;
    ReqQueue = reqQueue;
    IoLock = ioLock;
    Target = target;
    LunId = lun;
    CapacitySectors = capacity;
    SectorSz = sectorSize;
    ReqHdrSize = reqHdrSize;
    RespHdrSize = respHdrSize;

    ulong nameLen = Stdlib::StrLen(name);
    if (nameLen >= sizeof(DevName))
        nameLen = sizeof(DevName) - 1;
    Stdlib::MemCpy(DevName, name, nameLen);
    DevName[nameLen] = '\0';

    /* Allocate DMA pages for SCSI command buffers (reuse if already allocated).
       Page 0: VirtioScsiCmdReq (51 bytes) + VirtioScsiCmdResp (108 bytes)
       Page 1: data buffer (sector-sized) */
    if (!CmdReq)
    {
        ulong dmaPhys;
        void* dmaPtr = Mm::AllocMapPages(DmaPages, &dmaPhys);
        if (!dmaPtr)
        {
            Trace(0, "VirtioScsi %s: failed to alloc DMA pages", name);
            return false;
        }

        ulong dmaVirt = (ulong)dmaPtr;

        CmdReq = (VirtioScsiCmdReq*)dmaVirt;
        CmdReqPhys = dmaPhys;
        CmdResp = (VirtioScsiCmdResp*)(dmaVirt + sizeof(VirtioScsiCmdReq));
        CmdRespPhys = dmaPhys + sizeof(VirtioScsiCmdReq);
        DataBuf = (u8*)(dmaVirt + Const::PageSize);
        DataBufPhys = dmaPhys + Const::PageSize;
    }

    Initialized = true;
    return true;
}

/* --- Synchronous SCSI command (used for probing only) --- */

bool VirtioScsi::ScsiCommand(const u8 cdb[32], void* dataBuf, ulong dataLen, bool dataIn)
{
    if (!Initialized || !Transport || !ReqQueue)
        return false;

    if (dataLen > Const::PageSize)
    {
        Trace(0, "VirtioScsi %s: dataLen %u exceeds DMA buffer", DevName, dataLen);
        return false;
    }

    /* Build request header */
    Stdlib::MemSet(CmdReq, 0, sizeof(VirtioScsiCmdReq));
    EncodeLun(CmdReq->Lun, Target, LunId);
    CmdReq->Tag = 0;
    CmdReq->TaskAttr = 0; /* SIMPLE */
    CmdReq->Prio = 0;
    CmdReq->Crn = 0;
    Stdlib::MemCpy(CmdReq->Cdb, cdb, CdbLen);

    /* Clear response */
    Stdlib::MemSet(CmdResp, 0, sizeof(VirtioScsiCmdResp));

    /* Prepare data buffer */
    if (dataLen > 0 && !dataIn && dataBuf)
    {
        /* Write: copy data into DMA buffer */
        Stdlib::MemCpy(DataBuf, dataBuf, dataLen);
    }
    else if (dataLen > 0)
    {
        Stdlib::MemSet(DataBuf, 0, dataLen);
    }

    Barrier();

    /* Build descriptor chain (virtio-scsi spec 5.6.6.1):
       All device-readable (out) descriptors must come before device-writable (in).

       Data-out (write): CmdReq(R) -> DataBuf(R) -> CmdResp(W)
       Data-in  (read):  CmdReq(R) -> CmdResp(W) -> DataBuf(W)
       No data:          CmdReq(R) -> CmdResp(W) */

    if (dataLen > 0 && !dataIn)
    {
        /* Data-out (write): header, data, response */
        VirtQueue::BufDesc bufs[3];

        bufs[0].Addr = CmdReqPhys;
        bufs[0].Len = ReqHdrSize;
        bufs[0].Writable = false;

        bufs[1].Addr = DataBufPhys;
        bufs[1].Len = (u32)dataLen;
        bufs[1].Writable = false;

        bufs[2].Addr = CmdRespPhys;
        bufs[2].Len = RespHdrSize;
        bufs[2].Writable = true;

        int head = ReqQueue->AddBufs(bufs, 3);
        if (head < 0)
        {
            Trace(0, "VirtioScsi %s: AddBufs failed", DevName);
            return false;
        }
    }
    else if (dataLen > 0 && dataIn)
    {
        /* Data-in (read): header, response, data */
        VirtQueue::BufDesc bufs[3];

        bufs[0].Addr = CmdReqPhys;
        bufs[0].Len = ReqHdrSize;
        bufs[0].Writable = false;

        bufs[1].Addr = CmdRespPhys;
        bufs[1].Len = RespHdrSize;
        bufs[1].Writable = true;

        bufs[2].Addr = DataBufPhys;
        bufs[2].Len = (u32)dataLen;
        bufs[2].Writable = true;

        int head = ReqQueue->AddBufs(bufs, 3);
        if (head < 0)
        {
            Trace(0, "VirtioScsi %s: AddBufs failed", DevName);
            return false;
        }
    }
    else
    {
        /* No data transfer (e.g. TEST UNIT READY) */
        VirtQueue::BufDesc bufs[2];

        bufs[0].Addr = CmdReqPhys;
        bufs[0].Len = ReqHdrSize;
        bufs[0].Writable = false;

        bufs[1].Addr = CmdRespPhys;
        bufs[1].Len = RespHdrSize;
        bufs[1].Writable = true;

        int head = ReqQueue->AddBufs(bufs, 2);
        if (head < 0)
        {
            Trace(0, "VirtioScsi %s: AddBufs failed", DevName);
            return false;
        }
    }

    /* Notify device (request queue) */
    Transport->NotifyQueue(QueueRequest);

    /* Poll for completion */
    for (ulong i = 0; i < PollTimeout; i++)
    {
        if (ReqQueue->HasUsed())
            break;
        Pause();
    }

    u32 usedId, usedLen;
    if (!ReqQueue->GetUsed(usedId, usedLen))
    {
        Transport->ReadISR();
        Trace(0, "VirtioScsi %s: timeout waiting for SCSI command", DevName);
        return false;
    }

    /* Clear ISR so the device deasserts its level-triggered IRQ line.
       Without this, a shared IRQ (e.g. with virtio-net) would storm. */
    Transport->ReadISR();

    /* Check virtio-level response */
    if (CmdResp->Response != ResponseOk)
    {
        if (CmdResp->Response != ResponseBadTarget)
            Trace(0, "VirtioScsi %s: response error %u", DevName, (ulong)CmdResp->Response);
        return false;
    }

    /* Check SCSI status */
    if (CmdResp->Status != ScsiStatusGood)
    {
        Trace(0, "VirtioScsi %s: SCSI status 0x%p sense_len %u",
            DevName, (ulong)CmdResp->Status, (ulong)CmdResp->SenseLen);
        return false;
    }

    /* Copy data for read commands */
    if (dataLen > 0 && dataIn && dataBuf)
    {
        Stdlib::MemCpy(dataBuf, DataBuf, dataLen);
    }

    return true;
}

/* --- Async I/O path --- */

void VirtioScsi::Submit(BlockRequest* req)
{
    {
        Stdlib::AutoLock lock(QueueLock);
        RequestQueue.InsertTail(&req->Link);
    }
    DrainQueue();
}

void VirtioScsi::DrainQueue()
{
    if (!Initialized || !Hba)
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

        int slotIdx = Hba->AllocSlot();
        if (slotIdx < 0)
        {
            /* No free slots -- put requests back */
            Stdlib::AutoLock lock(QueueLock);
            RequestQueue.InsertHead(&req->Link);
            for (ulong j = i + 1; j < batchCount; j++)
                RequestQueue.InsertTail(&batch[j]->Link);
            break;
        }

        DmaSlot& slot = Hba->Slots[slotIdx];
        slot.Request = req;
        slot.Owner = this;

        /* Build SCSI CDB from BlockRequest */
        VirtioScsiCmdReq* cmdReq = slot.CmdReq;
        Stdlib::MemSet(cmdReq, 0, sizeof(VirtioScsiCmdReq));
        EncodeLun(cmdReq->Lun, Target, LunId);
        cmdReq->Tag = 0;
        cmdReq->TaskAttr = 0;
        cmdReq->Prio = 0;
        cmdReq->Crn = 0;

        u8* cdb = cmdReq->Cdb;
        Stdlib::MemSet(cdb, 0, CdbLen);

        /* Clear response */
        Stdlib::MemSet(slot.CmdResp, 0, sizeof(VirtioScsiCmdResp));

        if (req->RequestType == BlockRequest::Flush)
        {
            cdb[0] = ScsiOpSyncCache;
            /* Bytes 2-5: LBA = 0 (flush entire cache) */
            /* Bytes 7-8: number of blocks = 0 (flush all) */
        }
        else
        {
            if (req->RequestType == BlockRequest::Read)
                cdb[0] = ScsiOpRead10;
            else
                cdb[0] = ScsiOpWrite10;

            /* FUA bit: byte 1, bit 3 */
            if (req->Fua && req->RequestType == BlockRequest::Write)
                cdb[1] = 0x08;

            u32 lba = (u32)req->Sector;
            cdb[2] = (u8)(lba >> 24);
            cdb[3] = (u8)(lba >> 16);
            cdb[4] = (u8)(lba >> 8);
            cdb[5] = (u8)(lba);

            cdb[7] = (u8)(req->SectorCount >> 8);
            cdb[8] = (u8)(req->SectorCount);
        }

        Barrier();

        int head = -1;

        if (req->RequestType == BlockRequest::Flush)
        {
            /* No data: CmdReq(R) -> CmdResp(W) */
            VirtQueue::BufDesc bufs[2];

            bufs[0].Addr = slot.CmdReqPhys;
            bufs[0].Len = Hba->ReqHdrSize;
            bufs[0].Writable = false;

            bufs[1].Addr = slot.CmdRespPhys;
            bufs[1].Len = Hba->RespHdrSize;
            bufs[1].Writable = true;

            ulong flags = Hba->VirtQueueLock.LockIrqSave();
            head = Hba->ReqQueue.AddBufs(bufs, 2);
            Hba->VirtQueueLock.UnlockIrqRestore(flags);
        }
        else
        {
            ulong bufPhys = pt.VirtToPhys((ulong)req->Buffer);
            if (bufPhys == 0)
            {
                Trace(0, "VirtioScsi %s: VirtToPhys failed for buf 0x%p", DevName, (ulong)req->Buffer);
                req->Success = false;
                req->Completion.Done();
                Hba->FreeSlot(slotIdx);
                continue;
            }

            u32 dataLen = req->SectorCount * (u32)SectorSz;

            VirtQueue::BufDesc bufs[3];

            if (req->RequestType == BlockRequest::Write)
            {
                /* Data-out: CmdReq(R) -> DataBuf(R) -> CmdResp(W) */
                bufs[0].Addr = slot.CmdReqPhys;
                bufs[0].Len = Hba->ReqHdrSize;
                bufs[0].Writable = false;

                bufs[1].Addr = bufPhys;
                bufs[1].Len = dataLen;
                bufs[1].Writable = false;

                bufs[2].Addr = slot.CmdRespPhys;
                bufs[2].Len = Hba->RespHdrSize;
                bufs[2].Writable = true;
            }
            else
            {
                /* Data-in: CmdReq(R) -> CmdResp(W) -> DataBuf(W) */
                bufs[0].Addr = slot.CmdReqPhys;
                bufs[0].Len = Hba->ReqHdrSize;
                bufs[0].Writable = false;

                bufs[1].Addr = slot.CmdRespPhys;
                bufs[1].Len = Hba->RespHdrSize;
                bufs[1].Writable = true;

                bufs[2].Addr = bufPhys;
                bufs[2].Len = dataLen;
                bufs[2].Writable = true;
            }

            ulong flags = Hba->VirtQueueLock.LockIrqSave();
            head = Hba->ReqQueue.AddBufs(bufs, 3);
            Hba->VirtQueueLock.UnlockIrqRestore(flags);
        }

        if (head < 0)
        {
            /* Ring full -- return this and remaining requests to the queue */
            Hba->FreeSlot(slotIdx);
            Stdlib::AutoLock lock(QueueLock);
            RequestQueue.InsertHead(&req->Link);
            for (ulong j = i + 1; j < batchCount; j++)
                RequestQueue.InsertTail(&batch[j]->Link);
            break;
        }

        if ((ulong)head >= sizeof(Hba->SlotByHead) / sizeof(Hba->SlotByHead[0]))
        {
            Trace(0, "VirtioScsi %s: head %u out of range", DevName, (ulong)head);
            req->Success = false;
            req->Completion.Done();
            Hba->FreeSlot(slotIdx);
            continue;
        }

        slot.Head = head;
        Hba->SlotByHead[head] = &slot;
        kicked = true;
    }

    if (kicked)
        Hba->Transport.NotifyQueue(QueueRequest);
}

void VirtioScsi::DrainAllQueues()
{
    for (ulong i = 0; i < InstanceCount; i++)
        Instances[i].DrainQueue();
}

/* --- HBA-level slot management and completion --- */

int VirtioScsi::HbaState::AllocSlot()
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

void VirtioScsi::HbaState::FreeSlot(int idx)
{
    if (idx >= 0 && (ulong)idx < MaxSlots)
    {
        Slots[idx].Request = nullptr;
        Slots[idx].Owner = nullptr;
        Slots[idx].Head = -1;
        FreeSlotMask.SetBit((ulong)idx);
    }
}

void VirtioScsi::HbaState::CompleteIO()
{
    bool completed = false;

    for (;;)
    {
        u32 usedId, usedLen;

        ulong flags = VirtQueueLock.LockIrqSave();
        bool got = ReqQueue.GetUsed(usedId, usedLen);
        VirtQueueLock.UnlockIrqRestore(flags);

        if (!got)
            break;

        if (usedId >= sizeof(SlotByHead) / sizeof(SlotByHead[0]))
        {
            Trace(0, "VirtioScsi: bad used id %u", (ulong)usedId);
            continue;
        }

        DmaSlot* slot = SlotByHead[usedId];
        if (!slot || !slot->Request)
        {
            Trace(0, "VirtioScsi: no slot for used id %u", (ulong)usedId);
            continue;
        }

        SlotByHead[usedId] = nullptr;

        BlockRequest* req = slot->Request;

        /* Check response */
        if (slot->CmdResp->Response != ResponseOk || slot->CmdResp->Status != ScsiStatusGood)
            req->Success = false;
        else
            req->Success = true;

        int slotIdx = (int)(slot - Slots);
        FreeSlot(slotIdx);

        req->Completion.Done();
        completed = true;
    }

    if (completed)
        SoftIrq::GetInstance().Raise(SoftIrq::TypeBlkIo);
}

/* --- Block device operations using async path --- */

void VirtioScsi::WaitForCompletion(BlockRequest& req)
{
    if (GetInterruptsStarted())
    {
        req.Completion.Wait();
        return;
    }

    /* Early boot: interrupts not yet enabled, poll for completion */
    while (req.Completion.GetCounter() != 0)
    {
        if (Hba)
            Hba->CompleteIO();
        Pause(100);
    }
}

bool VirtioScsi::ReadSectors(u64 sector, void* buf, u32 count)
{
    if (count == 0)
        return true;
    if (sector + count > CapacitySectors)
        return false;
    if ((u64)SectorSz * count > Const::PageSize)
        return false;
    if ((ulong)buf & (Const::PageSize - 1))
    {
        Trace(0, "VirtioScsi %s: ReadSectors buf 0x%p not page-aligned", DevName, (ulong)buf);
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

bool VirtioScsi::WriteSectors(u64 sector, const void* buf, u32 count, bool fua)
{
    if (count == 0)
        return true;
    if (sector + count > CapacitySectors)
        return false;
    if ((u64)SectorSz * count > Const::PageSize)
        return false;
    if ((ulong)buf & (Const::PageSize - 1))
    {
        Trace(0, "VirtioScsi %s: WriteSectors buf 0x%p not page-aligned", DevName, (ulong)buf);
        return false;
    }

    BlockRequest req;
    req.RequestType = BlockRequest::Write;
    req.Fua = fua;
    req.Sector = sector;
    req.SectorCount = count;
    req.Buffer = (void*)buf;

    Submit(&req);
    WaitForCompletion(req);

    return req.Success;
}

bool VirtioScsi::Flush()
{
    if (!Initialized)
        return false;

    BlockRequest req;
    req.RequestType = BlockRequest::Flush;

    Submit(&req);
    WaitForCompletion(req);

    return req.Success;
}

const char* VirtioScsi::GetName()
{
    return DevName;
}

u64 VirtioScsi::GetCapacity()
{
    return CapacitySectors;
}

u64 VirtioScsi::GetSectorSize()
{
    return SectorSz;
}

void VirtioScsi::OnInterruptRegister(u8 irq, u8 vector)
{
    (void)irq;
    IntVector = vector;
    Trace(0, "VirtioScsi: interrupt registered vector 0x%p", (ulong)vector);
}

InterruptHandlerFn VirtioScsi::GetHandlerFn()
{
    return VirtioScsiInterruptStub;
}

void VirtioScsi::OnInterrupt(Context* ctx)
{
    Interrupt(ctx);
}

void VirtioScsi::Interrupt(Context* ctx)
{
    (void)ctx;

    if (!Hba)
    {
        /* During probing, just clear ISR */
        if (Transport)
            Transport->ReadISR();
        return;
    }

    u8 isr = Hba->Transport.ReadISR();
    if (isr == 0)
        return;

    InterruptCounter.Inc();
    InterruptStats::Inc(IrqVirtioScsi);

    /* Complete finished I/Os at HBA level */
    Hba->CompleteIO();
}

/* --- HBA and device initialization --- */

bool VirtioScsi::SetupHbaSlots(HbaState* hba)
{
    ulong dmaPhys;
    void* dmaPtr = Mm::AllocMapPages(1, &dmaPhys);
    if (!dmaPtr)
    {
        Trace(0, "VirtioScsi: failed to alloc DMA page for HBA slots");
        return false;
    }

    Stdlib::MemSet(dmaPtr, 0, Const::PageSize);
    ulong dmaVirt = (ulong)dmaPtr;

    /* Layout within the DMA page:
       MaxSlots * VirtioScsiCmdReq (51 bytes each) followed by
       MaxSlots * VirtioScsiCmdResp (108 bytes each).
       Total: MaxSlots * (51 + 108) = 8 * 159 = 1272 bytes. */
    for (ulong i = 0; i < MaxSlots; i++)
    {
        hba->Slots[i].CmdReq = (VirtioScsiCmdReq*)(dmaVirt + i * sizeof(VirtioScsiCmdReq));
        hba->Slots[i].CmdReqPhys = dmaPhys + i * sizeof(VirtioScsiCmdReq);
        hba->Slots[i].CmdResp = (VirtioScsiCmdResp*)(dmaVirt +
            MaxSlots * sizeof(VirtioScsiCmdReq) + i * sizeof(VirtioScsiCmdResp));
        hba->Slots[i].CmdRespPhys = dmaPhys +
            MaxSlots * sizeof(VirtioScsiCmdReq) + i * sizeof(VirtioScsiCmdResp);
        hba->Slots[i].Request = nullptr;
        hba->Slots[i].Owner = nullptr;
        hba->Slots[i].Head = -1;
    }

    hba->FreeSlotMask.Set((1L << MaxSlots) - 1);
    Stdlib::MemSet(hba->SlotByHead, 0, sizeof(hba->SlotByHead));

    return true;
}

bool VirtioScsi::InitHba(Pci::DeviceInfo* pciDev, HbaState* hba)
{
    auto& pci = Pci::GetInstance();

    pci.EnableBusMastering(pciDev->Bus, pciDev->Slot, pciDev->Func);

    if (!hba->Transport.Probe(pciDev))
    {
        Trace(0, "VirtioScsi: Transport.Probe failed");
        return false;
    }

    Trace(0, "VirtioScsi: %s virtio-pci probed, irq %u",
        hba->Transport.IsLegacy() ? "legacy" : "modern",
        (ulong)pciDev->InterruptLine);

    /* Reset */
    hba->Transport.Reset();

    /* Acknowledge */
    hba->Transport.SetStatus(VirtioPci::StatusAcknowledge);

    /* Driver */
    hba->Transport.SetStatus(VirtioPci::StatusAcknowledge | VirtioPci::StatusDriver);

    /* Feature negotiation */
    u32 devFeatures0 = hba->Transport.ReadDeviceFeature(0);
    Trace(0, "VirtioScsi: device features[0] 0x%p", (ulong)devFeatures0);

    hba->Transport.WriteDriverFeature(0, 0);

    if (!hba->Transport.IsLegacy())
    {
        u32 devFeatures1 = hba->Transport.ReadDeviceFeature(1);
        u32 drvFeatures1 = devFeatures1 & (1 << 0); /* VIRTIO_F_VERSION_1 */
        hba->Transport.WriteDriverFeature(1, drvFeatures1);
    }

    if (!hba->Transport.IsLegacy())
    {
        hba->Transport.SetStatus(VirtioPci::StatusAcknowledge | VirtioPci::StatusDriver |
                                  VirtioPci::StatusFeaturesOk);

        if (!(hba->Transport.GetStatus() & VirtioPci::StatusFeaturesOk))
        {
            Trace(0, "VirtioScsi: FEATURES_OK not set by device");
            hba->Transport.SetStatus(VirtioPci::StatusFailed);
            return false;
        }
    }

    /* Read device-specific configuration */
    u32 numQueues = hba->Transport.ReadDevCfg32(0);
    u32 segMax = hba->Transport.ReadDevCfg32(4);
    u32 maxSectors = hba->Transport.ReadDevCfg32(8);
    u32 cmdPerLun = hba->Transport.ReadDevCfg32(12);
    u32 senseSize = hba->Transport.ReadDevCfg32(20);
    u32 cdbSize = hba->Transport.ReadDevCfg32(24);
    u16 maxChannel = hba->Transport.ReadDevCfg8(28) | ((u16)hba->Transport.ReadDevCfg8(29) << 8);
    u16 maxTarget = hba->Transport.ReadDevCfg8(30) | ((u16)hba->Transport.ReadDevCfg8(31) << 8);
    u32 maxLun = hba->Transport.ReadDevCfg32(32);

    Trace(0, "VirtioScsi: numQueues %u segMax %u maxSectors %u cmdPerLun %u",
        (ulong)numQueues, (ulong)segMax, (ulong)maxSectors, (ulong)cmdPerLun);
    Trace(0, "VirtioScsi: cdbSize %u senseSize %u maxChannel %u maxTarget %u maxLun %u",
        (ulong)cdbSize, (ulong)senseSize, (ulong)maxChannel, (ulong)maxTarget, (ulong)maxLun);

    /* Clamp to our struct limits (CdbLen=32, SenseLen=96) */
    if (cdbSize == 0 || cdbSize > CdbLen)
        cdbSize = CdbLen;
    if (senseSize == 0 || senseSize > SenseLen)
        senseSize = SenseLen;

    hba->CdbSize = cdbSize;
    hba->SenseSize = senseSize;
    hba->MaxTarget = (maxTarget > 255) ? 255 : maxTarget;
    hba->ReqHdrSize = 19 + cdbSize;
    hba->RespHdrSize = 12 + senseSize;

    /* Queue 0 = control queue -- used for task management (abort, LUN reset).
       Not needed for basic synchronous I/O. Skip allocation. */
    hba->Transport.SelectQueue(QueueControl);

    /* Queue 1 = event queue -- used for async notifications (hotplug, live resize).
       Not needed without hotplug support. Skip allocation. */
    hba->Transport.SelectQueue(QueueEvent);

    /* Queue 2 = first request queue -- used for SCSI commands (read, write, inquiry).
       This is the only queue we need for block I/O. */
    hba->Transport.SelectQueue(QueueRequest);
    u16 queueSize = hba->Transport.GetQueueSize();
    Trace(0, "VirtioScsi: request queue size %u", (ulong)queueSize);

    if (queueSize == 0)
    {
        Trace(0, "VirtioScsi: request queue size is 0");
        hba->Transport.SetStatus(VirtioPci::StatusFailed);
        return false;
    }

    if (!hba->ReqQueue.Setup(queueSize))
    {
        Trace(0, "VirtioScsi: failed to setup request queue");
        hba->Transport.SetStatus(VirtioPci::StatusFailed);
        return false;
    }

    hba->Transport.SetQueueDesc(hba->ReqQueue.GetDescPhys());
    hba->Transport.SetQueueDriver(hba->ReqQueue.GetAvailPhys());
    hba->Transport.SetQueueDevice(hba->ReqQueue.GetUsedPhys());
    hba->Transport.EnableQueue();

    /* Set DRIVER_OK */
    u8 okStatus = VirtioPci::StatusAcknowledge | VirtioPci::StatusDriver |
                  VirtioPci::StatusDriverOk;
    if (!hba->Transport.IsLegacy())
        okStatus |= VirtioPci::StatusFeaturesOk;
    hba->Transport.SetStatus(okStatus);

    return true;
}

/* Probe a target/LUN using INQUIRY and READ CAPACITY.
   On success, registers a VirtioScsi instance as a BlockDevice. */
bool VirtioScsi::ProbeLun(HbaState* hba, u8 target, u16 lun)
{
    if (InstanceCount >= MaxInstances)
        return false;

    auto& inst = Instances[InstanceCount];

    /* Temporary init to use ScsiCommand for probing */
    char tmpName[8];
    tmpName[0] = 's';
    tmpName[1] = 'd';
    tmpName[2] = (char)('a' + InstanceCount);
    tmpName[3] = '\0';

    u32 reqHdrSize = 19 + hba->CdbSize;
    u32 respHdrSize = 12 + hba->SenseSize;
    if (!inst.Init(&hba->Transport, &hba->ReqQueue, &hba->Lock, target, lun, 0, DefaultSectorSize, tmpName,
                   reqHdrSize, respHdrSize))
        return false;

    /* INQUIRY: identify device type */
    u8 cdb[CdbLen];
    Stdlib::MemSet(cdb, 0, sizeof(cdb));
    cdb[0] = ScsiOpInquiry;
    cdb[4] = InquiryAllocLen;

    u8 inquiryData[InquiryAllocLen];
    Stdlib::MemSet(inquiryData, 0, sizeof(inquiryData));

    if (!inst.ScsiCommand(cdb, inquiryData, sizeof(inquiryData), true))
        return false;

    u8 deviceType = inquiryData[0] & InquiryTypeMask;
    u8 qualifier = (inquiryData[0] >> InquiryQualShift) & InquiryQualMask;

    Trace(0, "VirtioScsi %s: INQUIRY target %u lun %u type 0x%p qualifier 0x%p",
        tmpName, (ulong)target, (ulong)lun, (ulong)deviceType, (ulong)qualifier);

    /* Direct access block device with peripheral qualifier 0 (connected) */
    if (deviceType != ScsiTypeDirectAccess || qualifier != 0)
        return false;

    /* TEST UNIT READY: clear any pending UNIT ATTENTION conditions.
       After device init, the first command may get CHECK CONDITION with
       sense key UNIT ATTENTION.  Retry until the device reports GOOD. */
    for (ulong retry = 0; retry < 5; retry++)
    {
        Stdlib::MemSet(cdb, 0, sizeof(cdb));
        cdb[0] = ScsiOpTestUnitReady;
        if (inst.ScsiCommand(cdb, nullptr, 0, false))
            break;
    }

    /* READ CAPACITY(10): get sector count and sector size */
    Stdlib::MemSet(cdb, 0, sizeof(cdb));
    cdb[0] = ScsiOpReadCapacity;

    u8 capData[ReadCapRespLen];
    Stdlib::MemSet(capData, 0, sizeof(capData));

    if (!inst.ScsiCommand(cdb, capData, sizeof(capData), true))
        return false;

    /* READ CAPACITY(10) returns:
       Bytes 0-3: last LBA (big-endian)
       Bytes 4-7: block size (big-endian) */
    u32 lastLba = ((u32)capData[0] << 24) | ((u32)capData[1] << 16) |
                  ((u32)capData[2] << 8) | (u32)capData[3];
    u32 blockSize = ((u32)capData[4] << 24) | ((u32)capData[5] << 16) |
                    ((u32)capData[6] << 8) | (u32)capData[7];

    u64 capacity = (u64)lastLba + 1;

    Trace(0, "VirtioScsi %s: capacity %u sectors, block size %u (%u MB)",
        tmpName, capacity, (ulong)blockSize,
        (capacity * blockSize) / (1024 * 1024));

    if (blockSize == 0 || blockSize > Const::PageSize)
    {
        Trace(0, "VirtioScsi %s: unsupported block size %u", tmpName, (ulong)blockSize);
        return false;
    }

    inst.CapacitySectors = capacity;
    inst.SectorSz = blockSize;

    /* Register as block device */
    BlockDeviceTable::GetInstance().Register(&inst);

    InstanceCount++;
    return true;
}

void VirtioScsi::InitAll()
{
    auto& pci = Pci::GetInstance();
    InstanceCount = 0;
    HbaCount = 0;

    for (ulong i = 0; i < MaxInstances; i++)
        new (&Instances[i]) VirtioScsi();

    for (ulong i = 0; i < MaxHbas; i++)
        new (&Hbas[i]) HbaState();

    for (ulong i = 0; i < pci.GetDeviceCount() && HbaCount < MaxHbas; i++)
    {
        Pci::DeviceInfo* dev = pci.GetDevice(i);
        if (!dev)
            break;

        if (dev->Vendor != Pci::VendorVirtio)
            continue;
        if (dev->Device != Pci::DevVirtioScsi && dev->Device != Pci::DevVirtioScsiModern)
            continue;

        HbaState* hba = &Hbas[HbaCount];
        if (!InitHba(dev, hba))
            continue;

        /* Remember which instance index this HBA starts at */
        ulong firstInst = InstanceCount;

        /* Scan all targets, LUN 0 */
        for (u16 t = 0; t <= hba->MaxTarget && InstanceCount < MaxInstances; t++)
        {
            ProbeLun(hba, (u8)t, 0);
        }

        /* Free DMA pages left over from the last failed probe attempt.
           Instances[InstanceCount] is the slot used by ProbeLun but never
           committed (InstanceCount was not incremented). */
        if (InstanceCount < MaxInstances && Instances[InstanceCount].CmdReq &&
            InstanceCount >= firstInst)
        {
            Mm::UnmapFreePages(Instances[InstanceCount].CmdReq);
            Instances[InstanceCount].CmdReq = nullptr;
            Instances[InstanceCount].Initialized = false;
        }

        /* Free per-instance probe DMA pages and set up HBA slot pool.
           The per-instance DMA pages were used for synchronous probing;
           after this point, all I/O goes through the HBA slot pool. */
        for (ulong j = firstInst; j < InstanceCount; j++)
        {
            if (Instances[j].CmdReq)
            {
                Mm::UnmapFreePages(Instances[j].CmdReq);
                Instances[j].CmdReq = nullptr;
                Instances[j].CmdReqPhys = 0;
                Instances[j].CmdResp = nullptr;
                Instances[j].CmdRespPhys = 0;
                Instances[j].DataBuf = nullptr;
                Instances[j].DataBufPhys = 0;
            }
            Instances[j].Hba = hba;
            Instances[j].RequestQueue.Init();
        }

        if (InstanceCount > firstInst)
        {
            if (!SetupHbaSlots(hba))
            {
                Trace(0, "VirtioScsi: failed to setup HBA slot pool, disabling %u devices",
                      InstanceCount - firstInst);

                /* Clear Hba pointers so async I/O is rejected */
                for (ulong j = firstInst; j < InstanceCount; j++)
                    Instances[j].Hba = nullptr;
            }

            /* Register interrupt using the first instance discovered on this HBA */
            u8 irq = dev->InterruptLine;
            u8 vector = BaseVector + (u8)HbaCount;
            Interrupt::RegisterLevel(Instances[firstInst], irq, vector);
        }
        else
        {
            /* No devices found -- reset the device to deassert any
               pending level-triggered IRQ that could storm when
               interrupts are enabled (shared IRQ line with virtio-net). */
            hba->Transport.Reset();
        }

        HbaCount++;
    }

    Trace(0, "VirtioScsi: initialized %u devices on %u HBAs", InstanceCount, HbaCount);
}

/* Global interrupt handler called from assembly stub. */
extern "C" void VirtioScsiInterrupt(Context* ctx)
{
    for (ulong i = 0; i < VirtioScsi::InstanceCount; i++)
    {
        VirtioScsi::Instances[i].Interrupt(ctx);
    }

    Lapic::EOI();
}

}
