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

/* Virtio-SCSI command request header (virtio spec 5.6.6.1) */
struct VirtioScsiCmdReq
{
    u8  Lun[8];       /* SAM LUN representation */
    u64 Tag;          /* Command identifier */
    u8  TaskAttr;     /* Task attribute */
    u8  Prio;         /* SAM command priority */
    u8  Crn;          /* Command reference number */
    u8  Cdb[32];      /* SCSI CDB */
} __attribute__((packed));

static_assert(sizeof(VirtioScsiCmdReq) == 51, "Invalid size");

/* Virtio-SCSI command response (virtio spec 5.6.6.1) */
struct VirtioScsiCmdResp
{
    u32 SenseLen;          /* Sense data length */
    u32 Resid;             /* Residual bytes in data buffer */
    u16 StatusQualifier;   /* Status qualifier */
    u8  Status;            /* SCSI status (0 = GOOD) */
    u8  Response;          /* Virtio response (0 = OK) */
    u8  Sense[96];         /* Sense data */
} __attribute__((packed));

static_assert(sizeof(VirtioScsiCmdResp) == 108, "Invalid size");

class VirtioScsi : public BlockDevice, public InterruptHandler
{
public:
    VirtioScsi();
    virtual ~VirtioScsi();

    bool Init(VirtioPci* transport, VirtQueue* reqQueue, SpinLock* ioLock,
              u8 target, u16 lun, u64 capacity, u64 sectorSize, const char* name,
              u32 reqHdrSize, u32 respHdrSize);

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

    /* Discover and initialize all virtio-scsi devices. */
    static void InitAll();

    /* Virtio-SCSI response codes */
    static const u8 ResponseOk = 0;
    static const u8 ResponseBadTarget = 3;

    /* SCSI status codes */
    static const u8 ScsiStatusGood = 0;

    /* SCSI opcodes */
    static const u8 ScsiOpTestUnitReady = 0x00;
    static const u8 ScsiOpInquiry       = 0x12;
    static const u8 ScsiOpReadCapacity  = 0x25;
    static const u8 ScsiOpRead10        = 0x28;
    static const u8 ScsiOpWrite10       = 0x2A;

    /* SCSI device types */
    static const u8 ScsiTypeDirectAccess = 0;

    /* SCSI INQUIRY constants */
    static const u8 InquiryAllocLen     = 36;
    static const u8 InquiryTypeMask     = 0x1F;
    static const u8 InquiryQualShift    = 5;
    static const u8 InquiryQualMask     = 0x07;

    /* READ CAPACITY(10) response size */
    static const u8 ReadCapRespLen      = 8;

private:
    VirtioScsi(const VirtioScsi& other) = delete;
    VirtioScsi(VirtioScsi&& other) = delete;
    VirtioScsi& operator=(const VirtioScsi& other) = delete;
    VirtioScsi& operator=(VirtioScsi&& other) = delete;

    /* Encode SAM LUN representation */
    void EncodeLun(u8 lun[8], u8 target, u16 lunId);

    /* Send a SCSI command and wait for completion */
    bool ScsiCommand(const u8 cdb[32], void* dataBuf, ulong dataLen, bool dataIn);

    /* Read/write helper */
    bool DoIO(u64 sector, void* buf, bool read);

    VirtioPci* Transport;
    VirtQueue* ReqQueue;
    SpinLock* IoLock;          /* Points to shared HBA lock */
    u8 Target;
    u16 LunId;
    u64 CapacitySectors;
    u64 SectorSz;
    Atomic InterruptCounter;
    int IntVector;

    char DevName[8];
    bool Initialized;

    /* Descriptor lengths derived from device config */
    u32 ReqHdrSize;    /* 19 + cdb_size */
    u32 RespHdrSize;   /* 12 + sense_size */

    /* DMA buffers */
    VirtioScsiCmdReq* CmdReq;
    ulong CmdReqPhys;
    VirtioScsiCmdResp* CmdResp;
    ulong CmdRespPhys;
    u8* DataBuf;
    ulong DataBufPhys;

    static const ulong MaxInstances = 8;
    static const ulong PollTimeout  = 10000000;
    static const u8 BaseVector      = 0x35;
    static const ulong DmaPages     = 2;
    static const ulong DefaultSectorSize = 512;
    static const ulong CdbLen       = 32;   /* Must match VirtioScsiCmdReq::Cdb[] */
    static const ulong SenseLen     = 96;   /* Must match VirtioScsiCmdResp::Sense[] */

    /* Virtio-SCSI queue indices */
    static const u16 QueueControl   = 0;
    static const u16 QueueEvent     = 1;
    static const u16 QueueRequest   = 2;

    /* Shared transport and queue objects (one per PCI device) */
    static const ulong MaxHbas = 4;
    struct HbaState
    {
        VirtioPci Transport;
        VirtQueue ReqQueue;
        SpinLock Lock;        /* Serializes all I/O on this HBA */
        u32 CdbSize;          /* Actual CDB size from device config */
        u32 SenseSize;        /* Actual sense data size from device config */
        u16 MaxTarget;        /* Max target number from device config */
    };
    static HbaState Hbas[MaxHbas];
    static ulong HbaCount;

    /* Init helpers */
    static bool InitHba(Pci::DeviceInfo* pciDev, HbaState* hba);
    static bool ProbeLun(HbaState* hba, u8 target, u16 lun);

public:
    static VirtioScsi Instances[MaxInstances];
    static ulong InstanceCount;
};

}
