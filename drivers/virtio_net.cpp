#include "virtio_net.h"
#include "lapic.h"
#include "ioapic.h"
#include "acpi.h"

#include <kernel/trace.h>
#include <kernel/asm.h>
#include <kernel/panic.h>
#include <kernel/interrupt.h>
#include <kernel/idt.h>
#include <mm/page_table.h>
#include <mm/memory_map.h>
#include <mm/new.h>

namespace Kernel
{

VirtioNet VirtioNet::Instances[MaxInstances];
ulong VirtioNet::InstanceCount = 0;

VirtioNet::VirtioNet()
    : IoBase(0)
    , MyIp(0)
    , IntVector(-1)
    , Initialized(false)
    , TxBuf(nullptr)
    , TxBufPhys(0)
    , RxBufs(nullptr)
    , RxBufsPhys(0)
{
    DevName[0] = '\0';
    Stdlib::MemSet(MacAddr, 0, sizeof(MacAddr));
    Stdlib::MemSet(ArpCache, 0, sizeof(ArpCache));
}

VirtioNet::~VirtioNet()
{
}

u16 VirtioNet::Htons(u16 v)
{
    return (u16)((v >> 8) | (v << 8));
}

u32 VirtioNet::Htonl(u32 v)
{
    return ((v >> 24) & 0xFF) |
           ((v >> 8) & 0xFF00) |
           ((v << 8) & 0xFF0000) |
           ((v << 24) & 0xFF000000);
}

u16 VirtioNet::Ntohs(u16 v)
{
    return Htons(v);
}

u32 VirtioNet::Ntohl(u32 v)
{
    return Htonl(v);
}

u16 VirtioNet::IpChecksum(const void* data, ulong len)
{
    const u8* p = (const u8*)data;
    u32 sum = 0;

    for (ulong i = 0; i < len; i += 2)
    {
        u16 word;
        if (i + 1 < len)
            word = (u16)((p[i] << 8) | p[i + 1]);
        else
            word = (u16)(p[i] << 8);
        sum += word;
    }

    while (sum >> 16)
        sum = (sum & 0xFFFF) + (sum >> 16);

    return (u16)(~sum);
}

bool VirtioNet::Init(Pci::DeviceInfo* pciDev, const char* name)
{
    auto& pci = Pci::GetInstance();

    ulong nameLen = Stdlib::StrLen(name);
    if (nameLen >= sizeof(DevName))
        nameLen = sizeof(DevName) - 1;
    Stdlib::MemCpy(DevName, name, nameLen);
    DevName[nameLen] = '\0';

    /* Read BAR0 -- I/O port base */
    u32 bar0 = pci.GetBAR(pciDev->Bus, pciDev->Slot, pciDev->Func, 0);
    if (!(bar0 & 1))
    {
        Trace(0, "VirtioNet %s: BAR0 is MMIO, expected I/O port", name);
        return false;
    }
    IoBase = bar0 & 0xFFFC;

    Trace(0, "VirtioNet %s: BAR0 iobase 0x%p irq %u",
        name, (ulong)IoBase, (ulong)pciDev->InterruptLine);

    /* Enable PCI bus mastering */
    pci.EnableBusMastering(pciDev->Bus, pciDev->Slot, pciDev->Func);

    /* Reset device */
    Outb(IoBase + RegDeviceStatus, 0);

    /* Acknowledge */
    Outb(IoBase + RegDeviceStatus, StatusAcknowledge);

    /* Driver */
    Outb(IoBase + RegDeviceStatus, StatusAcknowledge | StatusDriver);

    /* Read and negotiate features */
    u32 devFeatures = In(IoBase + RegDeviceFeatures);
    Trace(0, "VirtioNet %s: device features 0x%p", name, (ulong)devFeatures);

    /* Request MAC feature */
    u32 guestFeatures = 0;
    if (devFeatures & FeatureMac)
        guestFeatures |= FeatureMac;
    Out(IoBase + RegGuestFeatures, guestFeatures);

    /* Setup RX virtqueue (queue 0) */
    Outw(IoBase + RegQueueSelect, 0);
    u16 rxQueueSize = Inw(IoBase + RegQueueSize);
    Trace(0, "VirtioNet %s: RX queue size %u", name, (ulong)rxQueueSize);

    if (rxQueueSize == 0)
    {
        Trace(0, "VirtioNet %s: RX queue size is 0", name);
        Outb(IoBase + RegDeviceStatus, StatusFailed);
        return false;
    }

    if (!RxQueue.Setup(rxQueueSize))
    {
        Trace(0, "VirtioNet %s: failed to setup RX queue", name);
        Outb(IoBase + RegDeviceStatus, StatusFailed);
        return false;
    }

    Out(IoBase + RegQueuePfn, (u32)(RxQueue.GetPhysAddr() / Const::PageSize));

    /* Setup TX virtqueue (queue 1) */
    Outw(IoBase + RegQueueSelect, 1);
    u16 txQueueSize = Inw(IoBase + RegQueueSize);
    Trace(0, "VirtioNet %s: TX queue size %u", name, (ulong)txQueueSize);

    if (txQueueSize == 0)
    {
        Trace(0, "VirtioNet %s: TX queue size is 0", name);
        Outb(IoBase + RegDeviceStatus, StatusFailed);
        return false;
    }

    if (!TxQueue.Setup(txQueueSize))
    {
        Trace(0, "VirtioNet %s: failed to setup TX queue", name);
        Outb(IoBase + RegDeviceStatus, StatusFailed);
        return false;
    }

    Outw(IoBase + RegQueueSelect, 1);
    Out(IoBase + RegQueuePfn, (u32)(TxQueue.GetPhysAddr() / Const::PageSize));

    /* Set DRIVER_OK */
    Outb(IoBase + RegDeviceStatus, StatusAcknowledge | StatusDriver | StatusDriverOk);

    /* Read MAC address from device config */
    if (guestFeatures & FeatureMac)
    {
        for (ulong i = 0; i < 6; i++)
            MacAddr[i] = Inb(IoBase + RegConfig + i);
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
        Outb(IoBase + RegDeviceStatus, StatusFailed);
        return false;
    }

    TxBufPhys = txPage->GetPhyAddress();
    for (ulong i = 0; i < 2; i++)
    {
        ulong va = txPage[i].GetPhyAddress() + Mm::MemoryMap::KernelSpaceBase;
        if (!pt.MapPage(va, &txPage[i]))
        {
            Trace(0, "VirtioNet %s: failed to map TX page %u", name, i);
            Outb(IoBase + RegDeviceStatus, StatusFailed);
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
        Outb(IoBase + RegDeviceStatus, StatusFailed);
        return false;
    }

    RxBufsPhys = rxPage->GetPhyAddress();
    for (ulong i = 0; i < rxPages; i++)
    {
        ulong va = rxPage[i].GetPhyAddress() + Mm::MemoryMap::KernelSpaceBase;
        if (!pt.MapPage(va, &rxPage[i]))
        {
            Trace(0, "VirtioNet %s: failed to map RX page %u", name, i);
            Outb(IoBase + RegDeviceStatus, StatusFailed);
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
    Interrupt::Register(*this, acpi.GetGsiByIrq(irq), vector);

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
    RxQueue.Kick(IoBase + RegQueueNotify, 0);
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

                if (etherType == 0x0806)
                {
                    ArpProcess(frame, frameLen);
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
        RxQueue.Kick(IoBase + RegQueueNotify, 0);
    }
}

/* --- ARP --- */

bool VirtioNet::ArpLookup(u32 ip, u8 mac[6])
{
    for (ulong i = 0; i < ArpCacheSize; i++)
    {
        if (ArpCache[i].Valid && ArpCache[i].Ip == ip)
        {
            Stdlib::MemCpy(mac, ArpCache[i].Mac, 6);
            return true;
        }
    }
    return false;
}

void VirtioNet::ArpInsert(u32 ip, const u8 mac[6])
{
    /* Look for existing entry first */
    for (ulong i = 0; i < ArpCacheSize; i++)
    {
        if (ArpCache[i].Valid && ArpCache[i].Ip == ip)
        {
            Stdlib::MemCpy(ArpCache[i].Mac, mac, 6);
            return;
        }
    }

    /* Find an empty slot */
    for (ulong i = 0; i < ArpCacheSize; i++)
    {
        if (!ArpCache[i].Valid)
        {
            ArpCache[i].Ip = ip;
            Stdlib::MemCpy(ArpCache[i].Mac, mac, 6);
            ArpCache[i].Valid = true;
            return;
        }
    }

    /* Cache full, overwrite first entry */
    ArpCache[0].Ip = ip;
    Stdlib::MemCpy(ArpCache[0].Mac, mac, 6);
    ArpCache[0].Valid = true;
}

void VirtioNet::ArpProcess(const u8* frame, ulong len)
{
    if (len < sizeof(EthHdr) + sizeof(ArpPacket))
        return;

    const ArpPacket* arp = (const ArpPacket*)(frame + sizeof(EthHdr));

    u16 opcode = Ntohs(arp->Opcode);

    if (opcode == 1) /* ARP Request */
    {
        /* If they're asking for our IP, send a reply */
        if (Ntohl(arp->TargetIp) == MyIp)
        {
            ArpSendReply(frame);
        }
        /* Also learn the sender's MAC */
        ArpInsert(Ntohl(arp->SenderIp), arp->SenderMac);
    }
    else if (opcode == 2) /* ARP Reply */
    {
        ArpInsert(Ntohl(arp->SenderIp), arp->SenderMac);
    }
}

void VirtioNet::ArpSendReply(const u8* reqFrame)
{
    const EthHdr* reqEth = (const EthHdr*)reqFrame;
    const ArpPacket* reqArp = (const ArpPacket*)(reqFrame + sizeof(EthHdr));

    /* Build reply in TX buffer */
    u8* pkt = TxBuf;
    Stdlib::MemSet(pkt, 0, sizeof(VirtioNetHdr) + sizeof(EthHdr) + sizeof(ArpPacket));

    /* Virtio net header (all zeros) */
    ulong off = sizeof(VirtioNetHdr);

    /* Ethernet header */
    EthHdr* eth = (EthHdr*)(pkt + off);
    Stdlib::MemCpy(eth->DstMac, reqEth->SrcMac, 6);
    Stdlib::MemCpy(eth->SrcMac, MacAddr, 6);
    eth->EtherType = Htons(0x0806);
    off += sizeof(EthHdr);

    /* ARP reply */
    ArpPacket* arp = (ArpPacket*)(pkt + off);
    arp->HwType = Htons(1);
    arp->ProtoType = Htons(0x0800);
    arp->HwSize = 6;
    arp->ProtoSize = 4;
    arp->Opcode = Htons(2); /* Reply */
    Stdlib::MemCpy(arp->SenderMac, MacAddr, 6);
    arp->SenderIp = Htonl(MyIp);
    Stdlib::MemCpy(arp->TargetMac, reqArp->SenderMac, 6);
    arp->TargetIp = reqArp->SenderIp;
    off += sizeof(ArpPacket);

    /* Send via TX queue */
    VirtQueue::BufDesc buf;
    buf.Addr = TxBufPhys;
    buf.Len = (u32)off;
    buf.Writable = false;

    Stdlib::AutoLock lock(TxLock);

    TxQueue.AddBufs(&buf, 1);
    TxQueue.Kick(IoBase + RegQueueNotify, 1);

    /* Poll for completion */
    for (ulong i = 0; i < 10000000; i++)
    {
        if (TxQueue.HasUsed())
            break;
        Pause();
    }

    u32 usedId, usedLen;
    TxQueue.GetUsed(usedId, usedLen);
}

bool VirtioNet::ArpRequest(u32 ip)
{
    /* Build ARP request in TX buffer */
    u8* pkt = TxBuf;
    Stdlib::MemSet(pkt, 0, sizeof(VirtioNetHdr) + sizeof(EthHdr) + sizeof(ArpPacket));

    ulong off = sizeof(VirtioNetHdr);

    /* Ethernet header -- broadcast */
    EthHdr* eth = (EthHdr*)(pkt + off);
    Stdlib::MemSet(eth->DstMac, 0xFF, 6);
    Stdlib::MemCpy(eth->SrcMac, MacAddr, 6);
    eth->EtherType = Htons(0x0806);
    off += sizeof(EthHdr);

    /* ARP request */
    ArpPacket* arp = (ArpPacket*)(pkt + off);
    arp->HwType = Htons(1);
    arp->ProtoType = Htons(0x0800);
    arp->HwSize = 6;
    arp->ProtoSize = 4;
    arp->Opcode = Htons(1); /* Request */
    Stdlib::MemCpy(arp->SenderMac, MacAddr, 6);
    arp->SenderIp = Htonl(MyIp);
    Stdlib::MemSet(arp->TargetMac, 0, 6);
    arp->TargetIp = Htonl(ip);
    off += sizeof(ArpPacket);

    /* Send */
    VirtQueue::BufDesc buf;
    buf.Addr = TxBufPhys;
    buf.Len = (u32)off;
    buf.Writable = false;

    {
        Stdlib::AutoLock lock(TxLock);

        TxQueue.AddBufs(&buf, 1);
        TxQueue.Kick(IoBase + RegQueueNotify, 1);

        for (ulong i = 0; i < 10000000; i++)
        {
            if (TxQueue.HasUsed())
                break;
            Pause();
        }

        u32 usedId, usedLen;
        TxQueue.GetUsed(usedId, usedLen);
    }

    TxPktCount.Inc();

    /* Wait for ARP reply -- poll RX a few times */
    for (ulong attempt = 0; attempt < 100; attempt++)
    {
        DrainRx();

        u8 mac[6];
        if (ArpLookup(ip, mac))
            return true;

        /* Brief delay */
        for (ulong j = 0; j < 100000; j++)
            Pause();
    }

    return false;
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

    TxQueue.Kick(IoBase + RegQueueNotify, 1);

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
    if (!ArpLookup(dstIp, dstMac))
    {
        if (!ArpRequest(dstIp))
        {
            Trace(0, "VirtioNet %s: ARP failed for 0x%p", DevName, (ulong)dstIp);
            /* Fall back to broadcast */
            Stdlib::MemSet(dstMac, 0xFF, 6);
        }
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
    Inb(IoBase + RegISRStatus);

    /* Drain RX queue */
    DrainRx();
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

        if (dev->Vendor != Pci::VendorVirtio || dev->Device != Pci::DevVirtioNetwork)
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
