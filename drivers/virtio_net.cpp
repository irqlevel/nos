#include "virtio_net.h"
#include "lapic.h"
#include "ioapic.h"

#include <kernel/trace.h>
#include <kernel/asm.h>
#include <kernel/panic.h>
#include <kernel/interrupt.h>
#include <kernel/idt.h>
#include <kernel/softirq.h>
#include <net/arp.h>
#include <net/icmp.h>
#include <net/tcp.h>
#include <mm/new.h>

namespace Kernel
{

using Net::EthHdr;
using Net::IpHdr;
using Net::UdpHdr;
using Net::ArpPacket;
using Net::Htons;
using Net::Htonl;
using Net::Ntohs;
using Net::Ntohl;
using Net::IpChecksum;

VirtioNet VirtioNet::Instances[MaxInstances];
ulong VirtioNet::InstanceCount = 0;

VirtioNet::VirtioNet()
    : RxNotifyAddr(nullptr)
    , TxNotifyAddr(nullptr)
    , IntVector(-1)
    , Initialized(false)
    , NetHdrSize(sizeof(VirtioNetHdr)) /* Updated in Init() for legacy */
    , FreeTxSlotMask(0)
    , TxHdrPage(nullptr)
    , TxHdrPagePhys(0)
    , RxBufs(nullptr)
    , RxBufsPhys(0)
    , RxNeedNotify(false)
{
    DevName[0] = '\0';
    Stdlib::MemSet(TxSlots, 0, sizeof(TxSlots));
    Stdlib::MemSet(TxSlotByHead, 0, sizeof(TxSlotByHead));
}

VirtioNet::~VirtioNet()
{
}

bool VirtioNet::Init(Pci::DeviceInfo* pciDev, const char* name)
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
        Trace(0, "VirtioNet %s: Transport.Probe failed", name);
        return false;
    }

    Trace(0, "VirtioNet %s: %s virtio-pci probed, irq %u",
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
    Trace(0, "VirtioNet %s: device features[0] 0x%p", name, (ulong)devFeatures0);

    u32 drvFeatures0 = 0;
    if (devFeatures0 & FeatureMac)
        drvFeatures0 |= FeatureMac;

    Transport.WriteDriverFeature(0, drvFeatures0);

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
            Trace(0, "VirtioNet %s: FEATURES_OK not set by device", name);
            Transport.SetStatus(VirtioPci::StatusFailed);
            return false;
        }
    }

    /* Legacy mode uses 10-byte header (no NumBuffers field) */
    NetHdrSize = Transport.IsLegacy() ? sizeof(VirtioNetHdrLegacy) : sizeof(VirtioNetHdr);
    Trace(0, "VirtioNet %s: net hdr size %u", name, NetHdrSize);

    /* Setup RX virtqueue (queue 0) */
    Transport.SelectQueue(0);
    u16 rxQueueSize = Transport.GetQueueSize();
    Trace(0, "VirtioNet %s: RX queue size %u", name, (ulong)rxQueueSize);

    if (rxQueueSize == 0)
    {
        Trace(0, "VirtioNet %s: RX queue size is 0", name);
        Transport.SetStatus(VirtioPci::StatusFailed);
        return false;
    }

    if (!HwRxQueue.Setup(rxQueueSize))
    {
        Trace(0, "VirtioNet %s: failed to setup RX queue", name);
        Transport.SetStatus(VirtioPci::StatusFailed);
        return false;
    }

    Transport.SetQueueDesc(HwRxQueue.GetDescPhys());
    Transport.SetQueueDriver(HwRxQueue.GetAvailPhys());
    Transport.SetQueueDevice(HwRxQueue.GetUsedPhys());
    Transport.EnableQueue();

    if (!Transport.IsLegacy())
        RxNotifyAddr = Transport.GetNotifyAddr(0);

    /* Setup TX virtqueue (queue 1) */
    Transport.SelectQueue(1);
    u16 txQueueSize = Transport.GetQueueSize();
    Trace(0, "VirtioNet %s: TX queue size %u", name, (ulong)txQueueSize);

    if (txQueueSize == 0)
    {
        Trace(0, "VirtioNet %s: TX queue size is 0", name);
        Transport.SetStatus(VirtioPci::StatusFailed);
        return false;
    }

    if (!HwTxQueue.Setup(txQueueSize))
    {
        Trace(0, "VirtioNet %s: failed to setup TX queue", name);
        Transport.SetStatus(VirtioPci::StatusFailed);
        return false;
    }

    Transport.SetQueueDesc(HwTxQueue.GetDescPhys());
    Transport.SetQueueDriver(HwTxQueue.GetAvailPhys());
    Transport.SetQueueDevice(HwTxQueue.GetUsedPhys());
    Transport.EnableQueue();

    if (!Transport.IsLegacy())
        TxNotifyAddr = Transport.GetNotifyAddr(1);

    /* Set DRIVER_OK */
    u8 okStatus = VirtioPci::StatusAcknowledge | VirtioPci::StatusDriver |
                  VirtioPci::StatusDriverOk;
    if (!Transport.IsLegacy())
        okStatus |= VirtioPci::StatusFeaturesOk;
    Transport.SetStatus(okStatus);

    /* Read MAC address from device config */
    if (drvFeatures0 & FeatureMac)
    {
        u8 macBytes[6];
        for (ulong i = 0; i < 6; i++)
            macBytes[i] = Transport.ReadDevCfg8(i);
        Mac = Net::MacAddress(macBytes);
    }

    Trace(0, "VirtioNet %s: MAC %p:%p:%p:%p:%p:%p",
        name,
        (ulong)Mac.Bytes[0], (ulong)Mac.Bytes[1], (ulong)Mac.Bytes[2],
        (ulong)Mac.Bytes[3], (ulong)Mac.Bytes[4], (ulong)Mac.Bytes[5]);

    /* Allocate DMA page for TX slot headers (8 slots, each NetHdrSize bytes) */
    TxHdrPage = (u8*)Mm::AllocMapPages(1, &TxHdrPagePhys);
    if (!TxHdrPage)
    {
        Trace(0, "VirtioNet %s: failed to alloc TX header page", name);
        Transport.SetStatus(VirtioPci::StatusFailed);
        return false;
    }
    Stdlib::MemSet(TxHdrPage, 0, Const::PageSize);

    /* Init TX slot pool */
    for (ulong s = 0; s < MaxTxSlots; s++)
    {
        TxSlots[s].HdrBuf = TxHdrPage + s * NetHdrSize;
        TxSlots[s].HdrBufPhys = TxHdrPagePhys + s * NetHdrSize;
        TxSlots[s].Frame = nullptr;
        TxSlots[s].Head = -1;
    }
    FreeTxSlotMask = (1UL << MaxTxSlots) - 1; /* all slots free */

    /* Allocate DMA pages for RX buffers (RxBufCount * RxBufSize = 32KB = 8 pages) */
    ulong rxPages = (RxBufCount * RxBufSize + Const::PageSize - 1) / Const::PageSize;
    RxBufs = (u8*)Mm::AllocMapPages(rxPages, &RxBufsPhys);
    if (!RxBufs)
    {
        Trace(0, "VirtioNet %s: failed to alloc RX DMA pages", name);
        Mm::UnmapFreePages(TxHdrPage);
        TxHdrPage = nullptr;
        Transport.SetStatus(VirtioPci::StatusFailed);
        return false;
    }

    /* Init pre-allocated RX frame descriptors */
    for (ulong r = 0; r < RxBufCount; r++)
    {
        RxFrames[r].Init();
        RxFrames[r].Direction = NetFrame::Rx;
        RxFrames[r].Release = RxFrameRelease;
        RxFrames[r].ReleaseCtx = this;
    }

    /* Default IP for QEMU user-mode networking */
    Ip = Net::IpAddress(10, 0, 2, 15);

    Initialized = true;

    /* Pre-post RX buffers */
    PostAllRxBufs();

    /* Register IRQ handler */
    u8 irq = pciDev->InterruptLine;
    u8 vector = 0x30 + (u8)InstanceCount;
    Interrupt::RegisterLevel(*this, irq, vector);

    /* Register as net device */
    NetDeviceTable::GetInstance().Register(this);

    Trace(0, "VirtioNet %s: initialized", name);
    return true;
}

void VirtioNet::PostRxBuf(ulong index)
{
    VirtQueue::BufDesc buf;
    buf.Addr = RxBufsPhys + index * RxBufSize;
    buf.Len = RxBufSize;
    buf.Writable = true;

    HwRxQueue.AddBufs(&buf, 1);
}

void VirtioNet::PostAllRxBufs()
{
    for (ulong i = 0; i < RxBufCount; i++)
        PostRxBuf(i);

    /* Notify device about available RX buffers */
    Transport.NotifyQueue(0);
}

/* --- TX slot management (caller holds TxQueueLock) --- */

int VirtioNet::AllocTxSlot()
{
    if (FreeTxSlotMask == 0)
        return -1;

    for (ulong i = 0; i < MaxTxSlots; i++)
    {
        if (FreeTxSlotMask & (1UL << i))
        {
            FreeTxSlotMask &= ~(1UL << i);
            return (int)i;
        }
    }
    return -1;
}

void VirtioNet::FreeTxSlot(int idx)
{
    TxSlots[idx].Frame = nullptr;
    TxSlots[idx].Head = -1;
    FreeTxSlotMask |= (1UL << (ulong)idx);
}

/* --- TX: classify frame for per-protocol stats --- */

void VirtioNet::ClassifyTxFrame(NetFrame* frame)
{
    if (frame->Length < sizeof(EthHdr))
        return;

    const EthHdr* eth = (const EthHdr*)frame->Data;
    u16 etherType = Ntohs(eth->EtherType);

    if (etherType == Net::EtherTypeArp)
    {
        TxArp.Inc();
        return;
    }

    if (etherType != Net::EtherTypeIp || frame->Length < sizeof(EthHdr) + sizeof(IpHdr))
    {
        TxOther.Inc();
        return;
    }

    const IpHdr* ip = (const IpHdr*)(frame->Data + sizeof(EthHdr));
    switch (ip->Protocol)
    {
    case Net::IpProtoIcmp: TxIcmp.Inc(); break;
    case Net::IpProtoTcp:  TxTcp.Inc();  break;
    case Net::IpProtoUdp:  TxUdp.Inc();  break;
    default:               TxOther.Inc(); break;
    }
}

/* --- TX: drain SW TxQueue to hardware (caller holds TxQueueLock) --- */

void VirtioNet::FlushTx()
{
    bool submitted = false;

    while (!TxQueue.IsEmpty())
    {
        int slotIdx = AllocTxSlot();
        if (slotIdx < 0)
            break; /* all DMA slots in-flight */

        Stdlib::ListEntry* entry = TxQueue.RemoveHead();
        TxCount--;
        NetFrame* frame = CONTAINING_RECORD(entry, NetFrame, Link);

        /* Fill slot header with zeroed virtio_net_hdr */
        Stdlib::MemSet(TxSlots[slotIdx].HdrBuf, 0, NetHdrSize);

        /* Build 2-descriptor chain: [hdr, data] */
        VirtQueue::BufDesc descs[2];
        descs[0].Addr = TxSlots[slotIdx].HdrBufPhys;
        descs[0].Len = (u32)NetHdrSize;
        descs[0].Writable = false;
        descs[1].Addr = frame->DataPhys;
        descs[1].Len = (u32)frame->Length;
        descs[1].Writable = false;

        int head = HwTxQueue.AddBufs(descs, 2);
        if (head < 0 || (ulong)head >= VirtQueue::MaxDescriptors)
        {
            FreeTxSlot(slotIdx);
            TxQueue.InsertHead(&frame->Link);
            TxCount++;
            break;
        }

        TxPktCount.Inc();
        ClassifyTxFrame(frame);

        TxSlots[slotIdx].Frame = frame;
        TxSlots[slotIdx].Head = head;
        TxSlotByHead[head] = &TxSlots[slotIdx];
        submitted = true;
    }

    if (submitted)
        Transport.NotifyQueue(1);

    if (!TxQueue.IsEmpty())
        SoftIrq::GetInstance().Raise(SoftIrq::TypeNetTx);
}

/* --- TX: complete hardware TX (caller holds TxQueueLock) --- */

void VirtioNet::CompleteTx()
{
    u32 usedId, usedLen;
    while (HwTxQueue.GetUsed(usedId, usedLen))
    {
        if (usedId >= VirtQueue::MaxDescriptors)
            continue;

        TxSlot* slot = TxSlotByHead[usedId];
        if (!slot)
            continue;

        NetFrame* frame = slot->Frame;
        FreeTxSlot((int)(slot - TxSlots));
        if (frame)
            frame->Put();
    }
}

/* --- TX: called from softirq to retry pending TX --- */

void VirtioNet::DrainTx()
{
    ulong flags = TxQueueLock.LockIrqSave();
    CompleteTx();
    FlushTx();
    TxQueueLock.UnlockIrqRestore(flags);
}

/* --- RX: reap completed buffers from HW into SW RxQueue --- */

void VirtioNet::ReapRx()
{
    while (HwRxQueue.HasUsed())
    {
        u32 usedId, usedLen;
        if (!HwRxQueue.GetUsed(usedId, usedLen))
            break;

        RxPktCount.Inc();

        if (usedId >= RxBufCount)
        {
            RxDropCount.Inc();
            continue;
        }

        if (usedLen <= NetHdrSize)
        {
            RxDropCount.Inc();
            PostRxBuf(usedId);
            RxNeedNotify = true;
            continue;
        }

        NetFrame* frame = &RxFrames[usedId];
        frame->Data = RxBufs + usedId * RxBufSize + NetHdrSize;
        frame->DataPhys = RxBufsPhys + usedId * RxBufSize + NetHdrSize;
        frame->Length = usedLen - NetHdrSize;
        frame->Refcount.Set(1);
        frame->Direction = NetFrame::Rx;
        frame->Release = RxFrameRelease;
        frame->ReleaseCtx = this;

        if (!EnqueueRx(frame))
        {
            RxDropCount.Inc();
            frame->Put(); /* reposts DMA buffer via RxFrameRelease */
        }
    }
}

/* --- RX: process frames from SW RxQueue (protocol dispatch) --- */

void VirtioNet::ProcessRx()
{
    while (true)
    {
        ulong flags = RxQueueLock.LockIrqSave();
        if (RxQueue.IsEmpty())
        {
            RxQueueLock.UnlockIrqRestore(flags);
            break;
        }
        Stdlib::ListEntry* entry = RxQueue.RemoveHead();
        RxCount--;
        RxQueueLock.UnlockIrqRestore(flags);

        NetFrame* frame = CONTAINING_RECORD(entry, NetFrame, Link);
        u8* data = frame->Data;
        ulong dataLen = frame->Length;

        /* Protocol dispatch */
        if (dataLen < sizeof(EthHdr))
        {
            RxDropCount.Inc();
            goto done;
        }

        {
            EthHdr* eth = (EthHdr*)data;
            u16 etherType = Ntohs(eth->EtherType);

            if (etherType == Net::EtherTypeArp)
            {
                RxArp.Inc();
                ArpTable::GetInstance().Process(this, data, dataLen);
                goto done;
            }

            if (etherType != Net::EtherTypeIp || dataLen < sizeof(EthHdr) + sizeof(IpHdr))
            {
                RxOther.Inc();
                RxDropCount.Inc();
                goto done;
            }

            IpHdr* ip = (IpHdr*)(data + sizeof(EthHdr));
            switch (ip->Protocol)
            {
            case Net::IpProtoIcmp:
                RxIcmp.Inc();
                Icmp::GetInstance().Process(this, data, dataLen);
                break;
            case Net::IpProtoTcp:
                RxTcp.Inc();
                Tcp::GetInstance().Process(this, data, dataLen);
                break;
            case Net::IpProtoUdp:
            {
                RxUdp.Inc();
                if (dataLen < sizeof(EthHdr) + sizeof(IpHdr) + sizeof(UdpHdr))
                {
                    RxDropCount.Inc();
                    break;
                }
                UdpHdr* udp = (UdpHdr*)(data + sizeof(EthHdr) + sizeof(IpHdr));
                u16 dstPort = Ntohs(udp->DstPort);
                bool delivered = false;

                Stdlib::AutoLock lock(UdpListenerLock);
                for (ulong li = 0; li < UdpListenerCount; li++)
                {
                    if (UdpListeners[li].Port == dstPort && UdpListeners[li].Cb)
                    {
                        UdpListeners[li].Cb(data, dataLen, UdpListeners[li].Ctx);
                        delivered = true;
                        break;
                    }
                }
                if (!delivered)
                    RxDropCount.Inc();
                break;
            }
            default:
                RxOther.Inc();
                RxDropCount.Inc();
                break;
            }
        }
done:

        frame->Put(); /* refcount -> 0 -> RxFrameRelease -> PostRxBuf */
    }

    if (RxNeedNotify)
    {
        Transport.NotifyQueue(0);
        RxNeedNotify = false;
    }
}

/* --- RX frame release callback --- */

void VirtioNet::RxFrameRelease(NetFrame* frame, void* ctx)
{
    VirtioNet* dev = (VirtioNet*)ctx;
    ulong idx = (ulong)(frame - dev->RxFrames);
    dev->PostRxBuf(idx);
    dev->RxNeedNotify = true;
}

bool VirtioNet::SendUdp(Net::IpAddress dstIp, u16 dstPort, Net::IpAddress srcIp, u16 srcPort,
                         const void* data, ulong len)
{
    if (!Initialized)
        return false;

    /* Resolve destination MAC via ARP.
       For off-subnet destinations, resolve the gateway MAC. */
    Net::IpAddress arpTarget = RouteIp(dstIp);
    Net::MacAddress dstMac;
    if (!ArpTable::GetInstance().Resolve(this, arpTarget, dstMac))
    {
        Trace(0, "VirtioNet %s: ARP failed for 0x%p", DevName, (ulong)dstIp.Addr4);
        /* Fall back to broadcast */
        dstMac = Net::MacAddress::Broadcast();
    }

    ulong udpLen = sizeof(UdpHdr) + len;
    ulong ipLen = sizeof(IpHdr) + udpLen;
    ulong frameLen = sizeof(EthHdr) + ipLen;

    if (frameLen > 1514) /* Ethernet MTU */
        return false;

    u8 frame[1514];
    Stdlib::MemSet(frame, 0, sizeof(frame));

    ulong off = 0;

    /* Ethernet header */
    EthHdr* eth = (EthHdr*)(frame + off);
    dstMac.CopyTo(eth->DstMac);
    Mac.CopyTo(eth->SrcMac);
    eth->EtherType = Htons(0x0800);
    off += sizeof(EthHdr);

    /* IP header */
    IpHdr* ip = (IpHdr*)(frame + off);
    ip->VersionIhl = 0x45; /* IPv4, IHL=5 */
    ip->Tos = 0;
    ip->TotalLen = Htons((u16)ipLen);
    ip->Id = 0;
    ip->FragOff = 0;
    ip->Ttl = 64;
    ip->Protocol = Net::IpProtoUdp;
    ip->Checksum = 0;
    ip->SrcAddr = srcIp.ToNetwork();
    ip->DstAddr = dstIp.ToNetwork();
    ip->Checksum = Htons(IpChecksum(ip, sizeof(IpHdr)));
    off += sizeof(IpHdr);

    /* UDP header */
    UdpHdr* udp = (UdpHdr*)(frame + off);
    udp->SrcPort = Htons(srcPort);
    udp->DstPort = Htons(dstPort);
    udp->Length = Htons((u16)udpLen);
    udp->Checksum = 0; /* Valid per RFC 768 */
    off += sizeof(UdpHdr);

    /* Payload */
    if (len > 0)
    {
        Stdlib::MemCpy(frame + off, data, len);
        off += len;
    }

    return SendRaw(frame, off);
}

/* --- Interface methods --- */

const char* VirtioNet::GetName()
{
    return DevName;
}

u64 VirtioNet::GetTxPackets()
{
    return (u64)TxPktCount.Get();
}

u64 VirtioNet::GetRxPackets()
{
    return (u64)RxPktCount.Get();
}

u64 VirtioNet::GetRxDropped()
{
    return (u64)RxDropCount.Get();
}

void VirtioNet::GetStats(NetStats& stats)
{
    stats.TxTotal = (u64)TxPktCount.Get();
    stats.RxTotal = (u64)RxPktCount.Get();
    stats.RxDrop = (u64)RxDropCount.Get();
    stats.RxIcmp = (u64)RxIcmp.Get();
    stats.RxUdp = (u64)RxUdp.Get();
    stats.RxTcp = (u64)RxTcp.Get();
    stats.RxArp = (u64)RxArp.Get();
    stats.RxOther = (u64)RxOther.Get();
    stats.TxIcmp = (u64)TxIcmp.Get();
    stats.TxUdp = (u64)TxUdp.Get();
    stats.TxTcp = (u64)TxTcp.Get();
    stats.TxArp = (u64)TxArp.Get();
    stats.TxOther = (u64)TxOther.Get();
}

/* --- Interrupt --- */

void VirtioNet::OnInterruptRegister(u8 irq, u8 vector)
{
    (void)irq;
    IntVector = vector;
    Trace(0, "VirtioNet %s: interrupt registered vector 0x%p", DevName, (ulong)vector);
}

InterruptHandlerFn VirtioNet::GetHandlerFn()
{
    return VirtioNetInterruptStub;
}

void VirtioNet::OnInterrupt(Context* ctx)
{
    /* Called by shared interrupt dispatch (no EOI here). */
    Interrupt(ctx);
}

void VirtioNet::Interrupt(Context* ctx)
{
    (void)ctx;

    /* Acknowledge interrupt */
    u8 isr = Transport.ReadISR();
    if (isr == 0)
        return;

    InterruptStats::Inc(IrqVirtioNet);

    /* Complete TX and try to drain pending frames under TxQueueLock */
    ulong flags = TxQueueLock.LockIrqSave();
    CompleteTx();
    FlushTx();
    TxQueueLock.UnlockIrqRestore(flags);

    /* Defer RX processing to the soft IRQ task */
    SoftIrq::GetInstance().Raise(SoftIrq::TypeNetRx);
}

/* --- Soft IRQ handlers --- */

static void NetRxSoftIrqHandler(void* ctx)
{
    (void)ctx;

    for (ulong i = 0; i < VirtioNet::InstanceCount; i++)
    {
        VirtioNet::Instances[i].ReapRx();
        VirtioNet::Instances[i].ProcessRx();
    }
}

static void NetTxSoftIrqHandler(void* ctx)
{
    (void)ctx;

    for (ulong i = 0; i < VirtioNet::InstanceCount; i++)
    {
        VirtioNet::Instances[i].DrainTx();
    }
}

/* --- InitAll --- */

void VirtioNet::InitAll()
{
    auto& pci = Pci::GetInstance();
    InstanceCount = 0;

    for (ulong i = 0; i < MaxInstances; i++)
        new (&Instances[i]) VirtioNet();

    for (ulong i = 0; i < pci.GetDeviceCount() && InstanceCount < MaxInstances; i++)
    {
        Pci::DeviceInfo* dev = pci.GetDevice(i);
        if (!dev)
            break;

        if (dev->Vendor != Pci::VendorVirtio)
            continue;
        if (dev->Device != Pci::DevVirtioNetwork && dev->Device != Pci::DevVirtioNetModern)
            continue;

        char name[8];
        name[0] = 'e';
        name[1] = 't';
        name[2] = 'h';
        name[3] = (char)('0' + InstanceCount);
        name[4] = '\0';

        VirtioNet& inst = Instances[InstanceCount];
        if (inst.Init(dev, name))
        {
            InstanceCount++;
        }
    }

    if (InstanceCount > 0)
    {
        SoftIrq::GetInstance().Register(SoftIrq::TypeNetRx, NetRxSoftIrqHandler, nullptr);
        SoftIrq::GetInstance().Register(SoftIrq::TypeNetTx, NetTxSoftIrqHandler, nullptr);
    }

    Trace(0, "VirtioNet: initialized %u devices", InstanceCount);
}

/* Global interrupt handler called from assembly stub. */
extern "C" void VirtioNetInterrupt(Context* ctx)
{
    for (ulong i = 0; i < VirtioNet::InstanceCount; i++)
    {
        VirtioNet::Instances[i].Interrupt(ctx);
    }

    Lapic::EOI();
}

}
