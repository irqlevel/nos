#include "virtio_net.h"
#include "lapic.h"
#include "ioapic.h"
#include "acpi.h"

#include <kernel/trace.h>
#include <kernel/asm.h>
#include <kernel/panic.h>
#include <kernel/interrupt.h>
#include <kernel/idt.h>
#include <kernel/softirq.h>
#include <net/arp.h>
#include <net/icmp.h>
#include <mm/page_table.h>
#include <mm/memory_map.h>
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
    , MyIp(0)
    , IntVector(-1)
    , Initialized(false)
    , RxCb(nullptr)
    , RxCbCtx(nullptr)
    , TxBuf(nullptr)
    , TxBufPhys(0)
    , RxBufs(nullptr)
    , RxBufsPhys(0)
{
    DevName[0] = '\0';
    Stdlib::MemSet(MacAddr, 0, sizeof(MacAddr));
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

    Trace(0, "VirtioNet %s: modern virtio-pci probed, irq %u",
        name, (ulong)pciDev->InterruptLine);

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
    /* features[1]: set VIRTIO_F_VERSION_1 (bit 32 = index 1 bit 0) */
    u32 devFeatures1 = Transport.ReadDeviceFeature(1);
    u32 drvFeatures1 = devFeatures1 & (1 << 0); /* VIRTIO_F_VERSION_1 */
    Transport.WriteDriverFeature(1, drvFeatures1);

    /* Set FEATURES_OK */
    Transport.SetStatus(VirtioPci::StatusAcknowledge | VirtioPci::StatusDriver |
                        VirtioPci::StatusFeaturesOk);

    /* Verify FEATURES_OK is still set */
    if (!(Transport.GetStatus() & VirtioPci::StatusFeaturesOk))
    {
        Trace(0, "VirtioNet %s: FEATURES_OK not set by device", name);
        Transport.SetStatus(VirtioPci::StatusFailed);
        return false;
    }

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

    if (!RxQueue.Setup(rxQueueSize))
    {
        Trace(0, "VirtioNet %s: failed to setup RX queue", name);
        Transport.SetStatus(VirtioPci::StatusFailed);
        return false;
    }

    Transport.SetQueueDesc(RxQueue.GetDescPhys());
    Transport.SetQueueDriver(RxQueue.GetAvailPhys());
    Transport.SetQueueDevice(RxQueue.GetUsedPhys());
    Transport.EnableQueue();

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

    if (!TxQueue.Setup(txQueueSize))
    {
        Trace(0, "VirtioNet %s: failed to setup TX queue", name);
        Transport.SetStatus(VirtioPci::StatusFailed);
        return false;
    }

    Transport.SetQueueDesc(TxQueue.GetDescPhys());
    Transport.SetQueueDriver(TxQueue.GetAvailPhys());
    Transport.SetQueueDevice(TxQueue.GetUsedPhys());
    Transport.EnableQueue();

    TxNotifyAddr = Transport.GetNotifyAddr(1);

    /* Set DRIVER_OK */
    Transport.SetStatus(VirtioPci::StatusAcknowledge | VirtioPci::StatusDriver |
                        VirtioPci::StatusFeaturesOk | VirtioPci::StatusDriverOk);

    /* Read MAC address from device config */
    if (drvFeatures0 & FeatureMac)
    {
        for (ulong i = 0; i < 6; i++)
            MacAddr[i] = Transport.ReadDevCfg8(i);
    }

    Trace(0, "VirtioNet %s: MAC %p:%p:%p:%p:%p:%p",
        name,
        (ulong)MacAddr[0], (ulong)MacAddr[1], (ulong)MacAddr[2],
        (ulong)MacAddr[3], (ulong)MacAddr[4], (ulong)MacAddr[5]);

    /* Allocate DMA pages for TX buffer (2 pages) */
    auto& pt = Mm::PageTable::GetInstance();
    Mm::Page* txPage = pt.AllocContiguousPages(2);
    if (!txPage)
    {
        Trace(0, "VirtioNet %s: failed to alloc TX DMA pages", name);
        Transport.SetStatus(VirtioPci::StatusFailed);
        return false;
    }

    TxBufPhys = txPage->GetPhyAddress();
    for (ulong i = 0; i < 2; i++)
    {
        ulong va = txPage[i].GetPhyAddress() + Mm::MemoryMap::KernelSpaceBase;
        if (!pt.MapPage(va, &txPage[i]))
        {
            Trace(0, "VirtioNet %s: failed to map TX page %u", name, i);
            Transport.SetStatus(VirtioPci::StatusFailed);
            return false;
        }
    }
    TxBuf = (u8*)(TxBufPhys + Mm::MemoryMap::KernelSpaceBase);

    /* Allocate DMA pages for RX buffers (RxBufCount * RxBufSize = 32KB = 8 pages) */
    ulong rxPages = (RxBufCount * RxBufSize + Const::PageSize - 1) / Const::PageSize;
    Mm::Page* rxPage = pt.AllocContiguousPages(rxPages);
    if (!rxPage)
    {
        Trace(0, "VirtioNet %s: failed to alloc RX DMA pages", name);
        Transport.SetStatus(VirtioPci::StatusFailed);
        return false;
    }

    RxBufsPhys = rxPage->GetPhyAddress();
    for (ulong i = 0; i < rxPages; i++)
    {
        ulong va = rxPage[i].GetPhyAddress() + Mm::MemoryMap::KernelSpaceBase;
        if (!pt.MapPage(va, &rxPage[i]))
        {
            Trace(0, "VirtioNet %s: failed to map RX page %u", name, i);
            Transport.SetStatus(VirtioPci::StatusFailed);
            return false;
        }
    }
    RxBufs = (u8*)(RxBufsPhys + Mm::MemoryMap::KernelSpaceBase);

    /* Default IP for QEMU user-mode networking */
    MyIp = (10 << 24) | (0 << 16) | (2 << 8) | 15; /* 10.0.2.15 */

    Initialized = true;

    /* Pre-post RX buffers */
    PostAllRxBufs();

    /* Register IRQ handler */
    u8 irq = pciDev->InterruptLine;
    u8 vector = 0x30 + (u8)InstanceCount;
    auto& acpi = Acpi::GetInstance();
    Interrupt::RegisterLevel(*this, acpi.GetGsiByIrq(irq), vector);

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

    RxQueue.AddBufs(&buf, 1);
}

void VirtioNet::PostAllRxBufs()
{
    for (ulong i = 0; i < RxBufCount; i++)
        PostRxBuf(i);

    /* Notify device about available RX buffers */
    RxQueue.Kick(RxNotifyAddr, 0);
}

void VirtioNet::DrainRx()
{
    bool reposted = false;

    while (RxQueue.HasUsed())
    {
        u32 usedId, usedLen;
        if (!RxQueue.GetUsed(usedId, usedLen))
            break;

        RxPktCount.Inc();

        /* The buffer contains virtio_net_hdr + ethernet frame.
           usedId is the descriptor index which maps to our buffer index. */
        if (usedId < RxBufCount && usedLen > sizeof(VirtioNetHdr))
        {
            u8* pkt = RxBufs + usedId * RxBufSize;
            u8* frame = pkt + sizeof(VirtioNetHdr);
            ulong frameLen = usedLen - sizeof(VirtioNetHdr);

            /* Check EtherType */
            if (frameLen >= sizeof(EthHdr))
            {
                EthHdr* eth = (EthHdr*)frame;
                u16 etherType = Ntohs(eth->EtherType);

                if (etherType == Net::EtherTypeArp)
                {
                    ArpTable::GetInstance().Process(this, frame, frameLen);
                }
                else if (etherType == Net::EtherTypeIp)
                {
                    if (frameLen >= sizeof(EthHdr) + sizeof(IpHdr))
                    {
                        IpHdr* ip = (IpHdr*)(frame + sizeof(EthHdr));

                        if (ip->Protocol == 1) /* ICMP */
                        {
                            Icmp::GetInstance().Process(this, frame, frameLen);
                        }
                        else if (ip->Protocol == 17) /* UDP */
                        {
                            /* Snapshot callback pointer under lock */
                            RxCallback cb;
                            void* cbCtx;
                            {
                                Stdlib::AutoLock lock(RxCbLock);
                                cb = RxCb;
                                cbCtx = RxCbCtx;
                            }

                            if (cb && frameLen >= sizeof(EthHdr) + sizeof(IpHdr) + sizeof(UdpHdr))
                            {
                                UdpHdr* udp = (UdpHdr*)(frame + sizeof(EthHdr) + sizeof(IpHdr));
                                if (Ntohs(udp->DstPort) == 68)
                                {
                                    cb(frame, frameLen, cbCtx);
                                }
                                else
                                {
                                    RxDropCount.Inc();
                                }
                            }
                            else
                            {
                                RxDropCount.Inc();
                            }
                        }
                        else
                        {
                            RxDropCount.Inc();
                        }
                    }
                    else
                    {
                        RxDropCount.Inc();
                    }
                }
                else
                {
                    RxDropCount.Inc();
                }
            }
            else
            {
                RxDropCount.Inc();
            }
        }
        else
        {
            RxDropCount.Inc();
        }

        /* Repost the buffer */
        if (usedId < RxBufCount)
        {
            PostRxBuf(usedId);
            reposted = true;
        }
    }

    if (reposted)
    {
        RxQueue.Kick(RxNotifyAddr, 0);
    }
}

/* --- Send --- */

bool VirtioNet::SendRaw(const void* buf, ulong len)
{
    if (!Initialized || len == 0)
        return false;

    /* Prepend virtio_net_hdr */
    ulong totalLen = sizeof(VirtioNetHdr) + len;
    if (totalLen > 2 * Const::PageSize)
        return false;

    Stdlib::AutoLock lock(TxLock);

    Stdlib::MemSet(TxBuf, 0, sizeof(VirtioNetHdr));
    Stdlib::MemCpy(TxBuf + sizeof(VirtioNetHdr), buf, len);

    VirtQueue::BufDesc desc;
    desc.Addr = TxBufPhys;
    desc.Len = (u32)totalLen;
    desc.Writable = false;

    int head = TxQueue.AddBufs(&desc, 1);
    if (head < 0)
    {
        Trace(0, "VirtioNet %s: AddBufs failed", DevName);
        return false;
    }

    TxQueue.Kick(TxNotifyAddr, 1);

    /* Poll for completion */
    for (ulong i = 0; i < 10000000; i++)
    {
        if (TxQueue.HasUsed())
            break;
        Pause();
    }

    u32 usedId, usedLen;
    if (!TxQueue.GetUsed(usedId, usedLen))
    {
        Trace(0, "VirtioNet %s: TX timeout", DevName);
        return false;
    }

    TxPktCount.Inc();
    return true;
}

bool VirtioNet::SendUdp(u32 dstIp, u16 dstPort, u32 srcIp, u16 srcPort,
                         const void* data, ulong len)
{
    if (!Initialized)
        return false;

    /* Resolve destination MAC via ARP */
    u8 dstMac[6];
    if (!ArpTable::GetInstance().Resolve(this, dstIp, dstMac))
    {
        Trace(0, "VirtioNet %s: ARP failed for 0x%p", DevName, (ulong)dstIp);
        /* Fall back to broadcast */
        Stdlib::MemSet(dstMac, 0xFF, 6);
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
    Stdlib::MemCpy(eth->DstMac, dstMac, 6);
    Stdlib::MemCpy(eth->SrcMac, MacAddr, 6);
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
    ip->Protocol = 17; /* UDP */
    ip->Checksum = 0;
    ip->SrcAddr = Htonl(srcIp);
    ip->DstAddr = Htonl(dstIp);
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

void VirtioNet::GetMac(u8 mac[6])
{
    Stdlib::MemCpy(mac, MacAddr, 6);
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

u32 VirtioNet::GetIp()
{
    return MyIp;
}

void VirtioNet::SetIp(u32 ip)
{
    MyIp = ip;
}

void VirtioNet::SetRxCallback(RxCallback cb, void* ctx)
{
    Stdlib::AutoLock lock(RxCbLock);
    RxCb = cb;
    RxCbCtx = ctx;
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

void VirtioNet::Interrupt(Context* ctx)
{
    (void)ctx;

    /* Acknowledge interrupt */
    Transport.ReadISR();

    /* Defer RX processing to the soft IRQ task */
    SoftIrq::GetInstance().Raise(SoftIrq::TypeNetRx);
}

/* --- Soft IRQ handler --- */

static void NetRxSoftIrqHandler(void* ctx)
{
    (void)ctx;

    for (ulong i = 0; i < VirtioNet::InstanceCount; i++)
    {
        VirtioNet::Instances[i].DrainRx();
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
