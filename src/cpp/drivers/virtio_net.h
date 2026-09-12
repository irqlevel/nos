#pragma once

#include <include/types.h>
#include <kernel/interrupt.h>
#include <net/net_device.h>
#include <net/net_frame.h>
#include <net/net_frame_pool.h>
#include <net/net.h>
#include <kernel/atomic.h>
#include <hal/context.h>
#include <drivers/virtqueue.h>
#include <drivers/pci.h>
#include <drivers/virtio_pci.h>
#include <drivers/virtio_mmio.h>

namespace Kernel
{

class VirtioNet : public NetDevice, public InterruptHandler
{
public:
    VirtioNet();
    virtual ~VirtioNet();

    bool Init(Pci::DeviceInfo* pciDev, const char* name);
    bool InitMmio(ulong base, ulong size, u32 intId, const char* name);

    /* NetDevice interface */
    virtual const char* GetName() override;
    virtual u64 GetTxPackets() override;
    virtual u64 GetRxPackets() override;
    virtual u64 GetRxDropped() override;
    virtual void GetStats(NetStats& stats) override;

    /* NetDevice TX/RX virtual methods */
    virtual void FlushTx() override;
    virtual void ProcessRx() override;

    /* InterruptHandler interface */
    virtual void OnInterruptRegister(u8 irq, u8 vector) override;
    virtual InterruptHandlerFn GetHandlerFn() override;
    virtual void OnInterrupt(Context* ctx) override;

    void Interrupt(Context* ctx);

    /* Called from the net RX softirq to reap received packets from HW */
    virtual void ReapRx() override;

    /* Called from the net TX softirq to retry pending TX */
    virtual void DrainTx() override;

    /* Discover and initialize all virtio-net devices. */
    static void InitAll();
    static void InitAllMmio(const VirtioMmioSlot* slots, ulong count);

private:
    VirtioNet(const VirtioNet& other) = delete;
    VirtioNet(VirtioNet&& other) = delete;
    VirtioNet& operator=(const VirtioNet& other) = delete;
    VirtioNet& operator=(VirtioNet&& other) = delete;

    /* Feature bits */
    static const u32 FeatureMac = (1 << 5); /* VIRTIO_NET_F_MAC */

    /* Virtio net header (v1.0 -- includes NumBuffers for VIRTIO_F_VERSION_1) */
    struct VirtioNetHdr
    {
        u8 Flags;
        u8 GsoType;
        u16 HdrLen;
        u16 GsoSize;
        u16 CsumStart;
        u16 CsumOffset;
        u16 NumBuffers;
    } __attribute__((packed));

    static_assert(sizeof(VirtioNetHdr) == 12, "Invalid size");

    /* Legacy virtio net header (without NumBuffers) */
    struct VirtioNetHdrLegacy
    {
        u8 Flags;
        u8 GsoType;
        u16 HdrLen;
        u16 GsoSize;
        u16 CsumStart;
        u16 CsumOffset;
    } __attribute__((packed));

    static_assert(sizeof(VirtioNetHdrLegacy) == 10, "Invalid size");

    /* RX: frames from the NetFramePool, each posted as a two-descriptor chain
       -- the virtio-net header into the slot's piece of RxHdrPage, the packet
       into the frame -- and handed up as they are once the device fills
       them, the slot refilled with a fresh frame on the spot. A frame handed
       up is then the stack's for as long as it likes, to release from
       anywhere: nothing about the ring waits for it. The zero-copy block
       server keeps one until the disk has written its payload, and sends it
       back afterwards as the reply. The sixteen static buffers this replaced
       were reposted by the frame's release, into a queue only the receive
       softirq may touch, so no frame could ever leave the receive path. */
    static const ulong MaxRxSlots = 128;
    static const ulong RxFrameSize = NetFramePool::FrameCapacity;
    static const ulong NoRxSlot = ~0UL;

    /* A fresh frame into an empty slot; false leaves it empty for RefillRx. */
    bool PostRxSlot(ulong slot);
    /* frame into slot; false if the queue had no room -- the frame is then
       still the caller's */
    bool PostRxFrame(ulong slot, NetFrame* frame);
    void RefillRx();

    /* TX DMA slot pool. Eight capped a burst at eight frames per round trip
       to the device -- a reply batch from the block server is dozens. */
    static const ulong MaxTxSlots = 32;

    struct TxSlot
    {
        u8* HdrBuf;
        ulong HdrBufPhys;
        NetFrame* Frame;
        int Head;
    };

    int AllocTxSlot();
    void FreeTxSlot(int idx);
    /* Hands every frame the device is done with to NetDevice::TxDone rather
       than releasing it: the caller holds TxQueueLock. See the note there. */
    void CompleteTx();

    VirtioPci PciTransport;
    VirtioMmio MmioTransport;
    VirtioTransport* Transport;

    bool InitCommon(const char* name, u8 irq, u8 vector);
    volatile void* RxNotifyAddr;
    volatile void* TxNotifyAddr;
    VirtQueue HwRxQueue;
    VirtQueue HwTxQueue;
    int IntVector;
    bool Initialized;
    ulong NetHdrSize; /* 10 for legacy, 12 for modern */
    char DevName[8];

    Atomic RxPktCount;
    Atomic RxDropCount;


    /* TX DMA slot pool */
    TxSlot TxSlots[MaxTxSlots];
    TxSlot* TxSlotByHead[VirtQueue::MaxDescriptors];
    ulong FreeTxSlotMask;
    u8* TxHdrPage;        /* one DMA page for all slot headers */
    ulong TxHdrPagePhys;

    /* RX slots: the frame each holds, nullptr while it is empty */
    ulong RxSlotCount;
    ulong RxEmptySlots;
    NetFrame* RxSlotFrame[MaxRxSlots];
    u8* RxHdrPage;        /* the device writes each slot's virtio-net header here */
    ulong RxHdrPagePhys;
    bool RxNeedNotify;

    /* Descriptor head -> RX slot. The virtqueue recycles freed descriptors
       in LIFO order, so heads and slots part company after the first lap. */
    ulong RxSlotByHead[VirtQueue::MaxDescriptors];

    static const ulong MaxInstances = 4;

public:
    static VirtioNet Instances[MaxInstances];
    static ulong InstanceCount;
};

}
