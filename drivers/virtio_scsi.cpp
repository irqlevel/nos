#include "virtio_scsi.h"
#include "lapic.h"
#include "ioapic.h"

#include <kernel/trace.h>
#include <kernel/asm.h>
#include <kernel/panic.h>
#include <kernel/interrupt.h>
#include <kernel/idt.h>
#include <mm/new.h>

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
    , CmdReq(nullptr)
    , CmdReqPhys(0)
    , CmdResp(nullptr)
    , CmdRespPhys(0)
    , DataBuf(nullptr)
    , DataBufPhys(0)
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
                       u8 target, u16 lun, u64 capacity, u64 sectorSize, const char* name)
{
    Transport = transport;
    ReqQueue = reqQueue;
    IoLock = ioLock;
    Target = target;
    LunId = lun;
    CapacitySectors = capacity;
    SectorSz = sectorSize;

    ulong nameLen = Stdlib::StrLen(name);
    if (nameLen >= sizeof(DevName))
        nameLen = sizeof(DevName) - 1;
    Stdlib::MemCpy(DevName, name, nameLen);
    DevName[nameLen] = '\0';

    /* Allocate DMA pages for SCSI command buffers.
       Page 0: VirtioScsiCmdReq (51 bytes) + VirtioScsiCmdResp (108 bytes)
       Page 1: data buffer (sector-sized) */
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

    Initialized = true;
    return true;
}

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
        bufs[0].Len = sizeof(VirtioScsiCmdReq);
        bufs[0].Writable = false;

        bufs[1].Addr = DataBufPhys;
        bufs[1].Len = (u32)dataLen;
        bufs[1].Writable = false;

        bufs[2].Addr = CmdRespPhys;
        bufs[2].Len = sizeof(VirtioScsiCmdResp);
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
        bufs[0].Len = sizeof(VirtioScsiCmdReq);
        bufs[0].Writable = false;

        bufs[1].Addr = CmdRespPhys;
        bufs[1].Len = sizeof(VirtioScsiCmdResp);
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
        bufs[0].Len = sizeof(VirtioScsiCmdReq);
        bufs[0].Writable = false;

        bufs[1].Addr = CmdRespPhys;
        bufs[1].Len = sizeof(VirtioScsiCmdResp);
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
        Trace(0, "VirtioScsi %s: timeout waiting for SCSI command", DevName);
        return false;
    }

    /* Check virtio-level response */
    if (CmdResp->Response != ResponseOk)
    {
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

bool VirtioScsi::DoIO(u64 sector, void* buf, bool read)
{
    if (!Initialized || !IoLock)
        return false;

    Stdlib::AutoLock lock(*IoLock);

    u8 cdb[CdbLen];
    Stdlib::MemSet(cdb, 0, sizeof(cdb));

    if (read)
        cdb[0] = ScsiOpRead10;
    else
        cdb[0] = ScsiOpWrite10;

    /* READ(10)/WRITE(10) CDB layout:
       Byte 0: opcode
       Byte 2-5: LBA (big-endian)
       Byte 7-8: transfer length in sectors (big-endian) */
    u32 lba = (u32)sector;
    cdb[2] = (u8)(lba >> 24);
    cdb[3] = (u8)(lba >> 16);
    cdb[4] = (u8)(lba >> 8);
    cdb[5] = (u8)(lba);

    /* Transfer 1 sector */
    cdb[7] = 0;
    cdb[8] = 1;

    return ScsiCommand(cdb, buf, (ulong)SectorSz, read);
}

bool VirtioScsi::ReadSector(u64 sector, void* buf)
{
    if (sector >= CapacitySectors)
        return false;
    return DoIO(sector, buf, true);
}

bool VirtioScsi::WriteSector(u64 sector, const void* buf)
{
    if (sector >= CapacitySectors)
        return false;
    return DoIO(sector, (void*)buf, false);
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
    InterruptCounter.Inc();
    Transport->ReadISR();
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

    if (!inst.Init(&hba->Transport, &hba->ReqQueue, &hba->Lock, target, lun, 0, DefaultSectorSize, tmpName))
        return false;

    /* INQUIRY: identify device type */
    u8 cdb[CdbLen];
    Stdlib::MemSet(cdb, 0, sizeof(cdb));
    cdb[0] = ScsiOpInquiry;
    cdb[4] = InquiryAllocLen;

    u8 inquiryData[InquiryAllocLen];
    Stdlib::MemSet(inquiryData, 0, sizeof(inquiryData));

    if (!inst.ScsiCommand(cdb, inquiryData, sizeof(inquiryData), true))
    {
        Mm::UnmapFreePages(inst.CmdReq);
        inst.Initialized = false;
        return false;
    }

    u8 deviceType = inquiryData[0] & InquiryTypeMask;
    u8 qualifier = (inquiryData[0] >> InquiryQualShift) & InquiryQualMask;

    Trace(0, "VirtioScsi %s: INQUIRY target %u lun %u type 0x%p qualifier 0x%p",
        tmpName, (ulong)target, (ulong)lun, (ulong)deviceType, (ulong)qualifier);

    /* Direct access block device with peripheral qualifier 0 (connected) */
    if (deviceType != ScsiTypeDirectAccess || qualifier != 0)
    {
        Mm::UnmapFreePages(inst.CmdReq);
        inst.Initialized = false;
        return false;
    }

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
    {
        Mm::UnmapFreePages(inst.CmdReq);
        inst.Initialized = false;
        return false;
    }

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
        Mm::UnmapFreePages(inst.CmdReq);
        inst.Initialized = false;
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

        /* Scan target 0, LUN 0 */
        ProbeLun(hba, 0, 0);

        /* Register interrupt using the first instance discovered on this HBA */
        if (InstanceCount > firstInst)
        {
            u8 irq = dev->InterruptLine;
            u8 vector = BaseVector + (u8)HbaCount;
            Interrupt::RegisterLevel(Instances[firstInst], irq, vector);
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
