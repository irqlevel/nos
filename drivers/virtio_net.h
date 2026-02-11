#pragma once

#include <include/types.h>
#include <kernel/interrupt.h>
#include <net/net_device.h>
#include <net/net.h>
#include <kernel/spin_lock.h>
#include <kernel/atomic.h>
#include <kernel/asm.h>
#include <drivers/virtqueue.h>
#include <drivers/pci.h>

namespace Kernel
{

class VirtioNet : public NetDevice, public InterruptHandler
{
public:
    VirtioNet();
    virtual ~VirtioNet();

    bool Init(Pci::DeviceInfo* pciDev, const char* name);

    /* NetDevice interface */
    virtual const char* GetName() override;
    virtual void GetMac(u8 mac[6]) override;
    virtual bool SendRaw(const void* buf, ulong len) override;
    virtual u64 GetTxPackets() override;
    virtual u64 GetRxPackets() override;
    virtual u64 GetRxDropped() override;
    virtual u32 GetIp() override;
    virtual void SetIp(u32 ip) override;
    virtual void SetRxCallback(RxCallback cb, void* ctx) override;

    /* Higher-level UDP send */
    bool SendUdp(u32 dstIp, u16 dstPort, u32 srcIp, u16 srcPort,
                 const void* data, ulong len);

    /* InterruptHandler interface */
    virtual void OnInterruptRegister(u8 irq, u8 vector) override;
    virtual InterruptHandlerFn GetHandlerFn() override;

    void Interrupt(Context* ctx);

    /* Called from soft IRQ task to process received packets */
    void DrainRx();

    /* Discover and initialize all virtio-net devices. */
    static void InitAll();

private:
    VirtioNet(const VirtioNet& other) = delete;
    VirtioNet(VirtioNet&& other) = delete;
    VirtioNet& operator=(const VirtioNet& other) = delete;
    VirtioNet& operator=(VirtioNet&& other) = delete;

    /* Virtio legacy PCI BAR0 register offsets */
    static const u16 RegDeviceFeatures = 0x00;
    static const u16 RegGuestFeatures  = 0x04;
    static const u16 RegQueuePfn       = 0x08;
    static const u16 RegQueueSize      = 0x0C;
    static const u16 RegQueueSelect    = 0x0E;
    static const u16 RegQueueNotify    = 0x10;
    static const u16 RegDeviceStatus   = 0x12;
    static const u16 RegISRStatus      = 0x13;
    static const u16 RegConfig         = 0x14;

    /* Device status bits */
    static const u8 StatusAcknowledge = 1;
    static const u8 StatusDriver      = 2;
    static const u8 StatusDriverOk    = 4;
    static const u8 StatusFailed      = 128;

    /* Feature bits */
    static const u32 FeatureMac = (1 << 5); /* VIRTIO_NET_F_MAC */

    /* Virtio net header (no mergeable buffers) */
    struct VirtioNetHdr
    {
        u8 Flags;
        u8 GsoType;
        u16 HdrLen;
        u16 GsoSize;
        u16 CsumStart;
        u16 CsumOffset;
    } __attribute__((packed));

    static_assert(sizeof(VirtioNetHdr) == 10, "Invalid size");

    /* RX buffer management */
    static const ulong RxBufCount = 16;
    static const ulong RxBufSize = 2048;
    void PostRxBuf(ulong index);
    void PostAllRxBufs();

    u16 IoBase;
    VirtQueue RxQueue;
    VirtQueue TxQueue;
    u8 MacAddr[6];
    u32 MyIp;
    SpinLock TxLock;
    int IntVector;
    bool Initialized;
    char DevName[8];

    Atomic TxPktCount;
    Atomic RxPktCount;
    Atomic RxDropCount;

    RxCallback RxCb;
    void* RxCbCtx;
    SpinLock RxCbLock;

    /* DMA buffers */
    u8* TxBuf;
    ulong TxBufPhys;
    u8* RxBufs;       /* RxBufCount * RxBufSize bytes */
    ulong RxBufsPhys;

    static const ulong MaxInstances = 4;

public:
    static VirtioNet Instances[MaxInstances];
    static ulong InstanceCount;
};

}
