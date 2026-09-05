#include "net_device.h"
#include "arp.h"
#include "icmp.h"
#include "tcp.h"

#include <kernel/trace.h>
#include <kernel/sched.h>
#include <kernel/parameters.h>
#include <kernel/softirq.h>
#include <kernel/panic.h>
#include <kernel/preempt.h>
#include <lib/stdlib.h>
#include <mm/new.h>

namespace Kernel
{

NetDevice::NetDevice()
    : TxCount(0)
    , RxCount(0)
    , UdpListenerCount(0)
{
    Stdlib::MemSet(RxProto, 0, sizeof(RxProto));
    Stdlib::MemSet(UdpListeners, 0, sizeof(UdpListeners));
}

void NetDevice::TxDone(NetFrame* frame)
{
    TxDoneQueue.InsertTail(&frame->Link);
}

void NetDevice::ReleaseTxDone()
{
    for (;;)
    {
        ulong flags = TxQueueLock.LockIrqSave();
        if (TxDoneQueue.IsEmpty())
        {
            TxQueueLock.UnlockIrqRestore(flags);
            return;
        }

        NetFrame* frame = CONTAINING_RECORD(TxDoneQueue.RemoveHead(), NetFrame, Link);
        TxQueueLock.UnlockIrqRestore(flags);

        /* Outside the lock, always: see the note on TxDone. */
        frame->Put();
    }
}

bool NetDevice::SubmitTx(NetFrame* frame)
{
    /* A panic report has to leave through this function, and the lock it
       needs may be held by a CPU that is never going to release it -- that
       is precisely the failure a panic is most often reporting. Blocking
       here means the report is never written, which is how a deadlocked TX
       path produces a machine that dies in complete silence.

       So once a panic has begun, take the lock if it is free and go on
       without it if it is not. Going on without it can race the holder into
       the driver's ring; on a machine that is already dying, a corrupted
       TX ring costs nothing and the report is worth everything. */
    bool acquired = true;
    ulong flags;

    if (Panicker::GetInstance().IsActive())
        flags = TxQueueLock.TryLockIrqSave(acquired);
    else
        flags = TxQueueLock.LockIrqSave();

    if (TxCount >= TxQueueCapacity)
    {
        if (acquired)
            TxQueueLock.UnlockIrqRestore(flags);
        else
            PreemptIrqRestore(flags);
        frame->Put();
        return false;
    }
    TxQueue.InsertTail(&frame->Link);
    TxCount++;
    FlushTx();

    if (acquired)
        TxQueueLock.UnlockIrqRestore(flags);
    else
        PreemptIrqRestore(flags);

    ReleaseTxDone();
    return true;
}

bool NetDevice::SendUdp(Net::IpAddress dstIp, u16 dstPort, Net::IpAddress srcIp, u16 srcPort,
                        const void* data, ulong len)
{
    /* Resolve the destination MAC via ARP. For an off-subnet destination
       RouteIp() hands back the gateway, so that is what gets resolved. */
    Net::IpAddress arpTarget = RouteIp(dstIp);
    Net::MacAddress dstMac;
    if (!ArpTable::GetInstance().Resolve(this, arpTarget, dstMac))
    {
        Trace(0, "NetDevice %s: ARP failed for 0x%p", GetName(), (ulong)dstIp.Addr4);
        /* Fall back to broadcast */
        dstMac = Net::MacAddress::Broadcast();
    }

    ulong udpLen = sizeof(Net::UdpHdr) + len;
    ulong ipLen = sizeof(Net::IpHdr) + udpLen;
    ulong frameLen = sizeof(Net::EthHdr) + ipLen;

    if (frameLen > 1514) /* Ethernet MTU */
        return false;

    u8 frame[1514];
    Stdlib::MemSet(frame, 0, sizeof(frame));

    ulong off = 0;

    /* Ethernet header */
    Net::EthHdr* eth = (Net::EthHdr*)(frame + off);
    dstMac.CopyTo(eth->DstMac);
    GetMac().CopyTo(eth->SrcMac);
    eth->EtherType = Net::Htons(0x0800);
    off += sizeof(Net::EthHdr);

    /* IP header */
    Net::IpHdr* ip = (Net::IpHdr*)(frame + off);
    ip->VersionIhl = 0x45; /* IPv4, IHL=5 */
    ip->Tos = 0;
    ip->TotalLen = Net::Htons((u16)ipLen);
    ip->Id = 0;
    ip->FragOff = 0;
    ip->Ttl = 64;
    ip->Protocol = Net::IpProtoUdp;
    ip->Checksum = 0;
    ip->SrcAddr = srcIp.ToNetwork();
    ip->DstAddr = dstIp.ToNetwork();
    ip->Checksum = Net::Htons(Net::IpChecksum(ip, sizeof(Net::IpHdr)));
    off += sizeof(Net::IpHdr);

    /* UDP header */
    Net::UdpHdr* udp = (Net::UdpHdr*)(frame + off);
    udp->SrcPort = Net::Htons(srcPort);
    udp->DstPort = Net::Htons(dstPort);
    udp->Length = Net::Htons((u16)udpLen);
    udp->Checksum = 0; /* Valid per RFC 768 */
    off += sizeof(Net::UdpHdr);

    /* Payload */
    if (len > 0)
    {
        Stdlib::MemCpy(frame + off, data, len);
        off += len;
    }

    return SendRaw(frame, off);
}

bool NetDevice::SendRaw(const void* buf, ulong len)
{
    if (len == 0)
        return false;

    NetFrame* frame = NetFrame::AllocTx(len);
    if (!frame)
        return false;

    Stdlib::MemCpy(frame->Data, buf, len);
    frame->Length = len;
    return SubmitTx(frame);
}

void NetDevice::DrainTx()
{
    ulong flags = TxQueueLock.LockIrqSave();
    if (TxCount > 0)
        FlushTx();
    TxQueueLock.UnlockIrqRestore(flags);

    ReleaseTxDone();
}

void NetDevice::GetRxProtoTotals(NetStats& stats)
{
    stats.RxIcmp = 0;
    stats.RxUdp = 0;
    stats.RxTcp = 0;
    stats.RxArp = 0;
    stats.RxOther = 0;
    stats.RxDrop = 0;

    for (ulong i = 0; i < MaxCpus; i++)
    {
        stats.RxIcmp += RxProto[i].Icmp;
        stats.RxUdp += RxProto[i].Udp;
        stats.RxTcp += RxProto[i].Tcp;
        stats.RxArp += RxProto[i].Arp;
        stats.RxOther += RxProto[i].Other;
        stats.RxDrop += RxProto[i].Drop;
    }
}

ulong NetDevice::EnqueueRxBatch(NetFrame** frames, ulong count)
{
    /* One acquisition for the whole harvest, rather than one per frame.
       Returns how many were taken; the caller releases the rest. */
    ulong taken = 0;

    ulong flags = RxQueueLock.LockIrqSave();
    while (taken < count && RxCount < RxQueueCapacity)
    {
        RxQueue.InsertTail(&frames[taken]->Link);
        RxCount++;
        taken++;
    }
    RxQueueLock.UnlockIrqRestore(flags);

    return taken;
}

bool NetDevice::EnqueueRx(NetFrame* frame)
{
    ulong flags = RxQueueLock.LockIrqSave();
    if (RxCount >= RxQueueCapacity)
    {
        RxQueueLock.UnlockIrqRestore(flags);
        return false;
    }
    RxQueue.InsertTail(&frame->Link);
    RxCount++;
    RxQueueLock.UnlockIrqRestore(flags);
    return true;
}

bool NetDevice::RegisterUdpListener(u16 port, RxCallback cb, void* ctx)
{
    Stdlib::AutoLock lock(UdpListenerLock);

    for (ulong i = 0; i < UdpListenerCount; i++)
    {
        if (UdpListeners[i].Port == port)
        {
            UdpListeners[i].Cb = cb;
            UdpListeners[i].Ctx = ctx;
            return true;
        }
    }

    if (UdpListenerCount >= MaxUdpListeners)
        return false;

    UdpListeners[UdpListenerCount].Port = port;
    UdpListeners[UdpListenerCount].Cb = cb;
    UdpListeners[UdpListenerCount].Ctx = ctx;
    UdpListenerCount++;
    return true;
}

void NetDevice::UnregisterUdpListener(u16 port)
{
    {
        Stdlib::AutoLock lock(UdpListenerLock);

        for (ulong i = 0; i < UdpListenerCount; i++)
        {
            if (UdpListeners[i].Port == port)
            {
                for (ulong j = i; j + 1 < UdpListenerCount; j++)
                    UdpListeners[j] = UdpListeners[j + 1];
                UdpListenerCount--;
                Stdlib::MemSet(&UdpListeners[UdpListenerCount], 0, sizeof(UdpListener));
                break;
            }
        }
    }

    /* No new dispatch can find the listener now; wait out any that took it
       before the lock, so the caller may free its context on return. The
       count covers every listener on the device, not just this port -- the
       table is compacted on removal, so a per-slot count would not stay
       with its slot -- and a callback is microseconds, so waiting for a
       neighbour's costs nothing worth a second data structure. Task context
       only: a listener that unregistered itself from inside its own
       callback would wait here for itself. */
    while (UdpListenerInFlight.Get() != 0)
        Sleep(1 * Const::NanoSecsInMs);
}

Net::MacAddress NetDevice::GetMac()
{
    return Mac;
}

void NetDevice::SetMac(const Net::MacAddress& mac)
{
    Mac = mac;
}

Net::IpAddress NetDevice::GetIp()
{
    return Ip;
}

void NetDevice::SetIp(Net::IpAddress ip)
{
    Ip = ip;
}

Net::IpAddress NetDevice::GetSubnetMask()
{
    return Mask;
}

void NetDevice::SetSubnetMask(Net::IpAddress mask)
{
    Mask = mask;
}

Net::IpAddress NetDevice::GetGateway()
{
    return Gw;
}

void NetDevice::SetGateway(Net::IpAddress gw)
{
    Gw = gw;
}

Net::IpAddress NetDevice::RouteIp(Net::IpAddress dstIp)
{
    if (Mask.Addr4 != 0 && Gw.Addr4 != 0)
    {
        if ((dstIp.Addr4 & Mask.Addr4) != (Ip.Addr4 & Mask.Addr4))
            return Gw;
    }
    return dstIp;
}

NetDeviceTable::NetDeviceTable()
    : Count(0)
{
    LastPassWasPoll = false;

    for (ulong i = 0; i < MaxDevices; i++)
        Devices[i] = nullptr;
}

NetDeviceTable::~NetDeviceTable()
{
}

static void NetRxSoftIrqHandler(void* ctx)
{
    (void)ctx;
    NetDeviceTable::GetInstance().ProcessAllRx();
}

static void NetTxSoftIrqHandler(void* ctx)
{
    (void)ctx;
    NetDeviceTable::GetInstance().ProcessAllTx();
}

bool NetDeviceTable::Register(NetDevice* dev)
{
    if (Count >= MaxDevices || dev == nullptr)
        return false;

    Devices[Count] = dev;
    Count++;

    if (Count == 1)
    {
        /* One handler per softirq type (SoftIrq allows a single handler),
           dispatching RX/TX to every registered device -- virtio-net and
           Rust drivers alike raise TypeNetRx/TypeNetTx from their ISRs. */
        SoftIrq::GetInstance().Register(SoftIrq::TypeNetRx, NetRxSoftIrqHandler, nullptr);
        SoftIrq::GetInstance().Register(SoftIrq::TypeNetTx, NetTxSoftIrqHandler, nullptr);
    }

    Net::MacAddress mac = dev->GetMac();

    Trace(0, "NetDevice registered: %s mac %p:%p:%p:%p:%p:%p",
        dev->GetName(),
        (ulong)mac.Bytes[0], (ulong)mac.Bytes[1], (ulong)mac.Bytes[2],
        (ulong)mac.Bytes[3], (ulong)mac.Bytes[4], (ulong)mac.Bytes[5]);

    return true;
}

void NetDevice::DrainRxQueueAndDispatch()
{
    /* Take the whole queue in one go, then dispatch with the lock down.
       Frames arrive one at a time but they are dispatched in a run, and the
       queue lock was being taken and released -- with interrupts disabled --
       once per packet on each side of it. At 37000 packets a second that
       showed up in a profile as the top of the receive path: two acquisitions
       here and one in EnqueueRx, per frame, on the one CPU doing all of it.
       Splicing the list costs one acquisition per batch instead. */
    Stdlib::ListEntry batch;
    batch.Init();

    {
        ulong flags = RxQueueLock.LockIrqSave();
        batch.MoveTailList(&RxQueue);
        RxCount = 0;
        RxQueueLock.UnlockIrqRestore(flags);
    }

    /* Nothing arrived: no table to copy, no hold to take, no lock. The drain
       runs on every softirq pass, most of which have no frames. */
    if (batch.IsEmpty())
        return;

    /* One look at the listener table for the whole batch. It was a spinlock
       acquire and release per UDP datagram -- with interrupts off, plus the
       pair of atomics on the in-flight count -- which a profile put among the
       top entries of the receive path at 37000 packets a second. The table
       has four slots and changes when a server starts or stops, so copying it
       per batch costs nothing and the copy is good for the length of one.

       The in-flight count is raised once for the batch and dropped at the
       end, which is what lets UnregisterUdpListener keep waiting for
       callbacks to finish before its caller frees their context. */
    /* Read once for the batch: the counters below are per CPU, and the poll
       that produced this batch does not migrate part way through it. */
    ulong cpuIndex = Hal::GetCurrentCpuHwId();
    if (cpuIndex >= MaxCpus)
        cpuIndex = 0;
    RxProtoCounters& proto = RxProto[cpuIndex];

    UdpListener listeners[MaxUdpListeners];
    ulong listenerCount = 0;

    {
        Stdlib::AutoLock lock(UdpListenerLock);
        listenerCount = UdpListenerCount;
        for (ulong i = 0; i < listenerCount; i++)
            listeners[i] = UdpListeners[i];
        if (listenerCount != 0)
            UdpListenerInFlight.Inc();
    }

    while (!batch.IsEmpty())
    {
        Stdlib::ListEntry* entry = batch.RemoveHead();

        NetFrame* frame = CONTAINING_RECORD(entry, NetFrame, Link);
        u8* data = frame->Data;
        ulong dataLen = frame->Length;

        if (dataLen < sizeof(Net::EthHdr))
        {
            proto.Drop++;
            goto done;
        }

        {
            Net::EthHdr* eth = (Net::EthHdr*)data;
            u16 etherType = Net::Ntohs(eth->EtherType);

            if (etherType == Net::EtherTypeArp)
            {
                proto.Arp++;
                ArpTable::GetInstance().Process(this, data, dataLen);
                goto done;
            }

            if (etherType != Net::EtherTypeIp ||
                dataLen < sizeof(Net::EthHdr) + sizeof(Net::IpHdr))
            {
                proto.Other++;
                proto.Drop++;
                goto done;
            }

            Net::IpHdr* ip = (Net::IpHdr*)(data + sizeof(Net::EthHdr));
            switch (ip->Protocol)
            {
            case Net::IpProtoIcmp:
                proto.Icmp++;
                Icmp::GetInstance().Process(this, data, dataLen);
                break;
            case Net::IpProtoTcp:
                proto.Tcp++;
                Tcp::GetInstance().Process(this, data, dataLen);
                break;
            case Net::IpProtoUdp:
            {
                proto.Udp++;
                ulong ipHdrLen = Net::IpHeaderLen(ip);
                if (ipHdrLen == 0 ||
                    dataLen < sizeof(Net::EthHdr) + ipHdrLen + sizeof(Net::UdpHdr))
                    break;
                Net::UdpHdr* udp = (Net::UdpHdr*)(data + sizeof(Net::EthHdr) + ipHdrLen);
                u16 dstPort = Net::Ntohs(udp->DstPort);

                /* The callback ran under UdpListenerLock -- a spinlock, so
                   with interrupts off -- on every datagram. On the CPU the
                   NIC's MSI-X targets that held the card's own interrupt
                   back for the length of every callback, and forbade the
                   callback anything that might block. Take the listener out
                   under the lock, then call it with the lock down; the
                   in-flight count is what keeps the context alive until it
                   returns. */
                for (ulong li = 0; li < listenerCount; li++)
                {
                    if (listeners[li].Port == dstPort && listeners[li].Cb)
                    {
                        listeners[li].Cb(data, dataLen, listeners[li].Ctx);
                        break;
                    }
                }
                break;
            }
            default:
                proto.Other++;
                proto.Drop++;
                break;
            }
        }
done:
        frame->Put();
    }

    if (listenerCount != 0)
        UdpListenerInFlight.Dec();
}

void NetDeviceTable::ProcessAllRx()
{
    /* Test and clear: whoever gets the 1 owns the attribution for this pass. */
    bool polled = (PollPending.Cmpxchg(0, 1) == 1);
    ulong pending = 0;

    for (ulong i = 0; i < Count; i++)
    {
        Devices[i]->ReapRx();

        /* After the reap, before the dispatch: what the hardware had waiting. */
        pending += Devices[i]->GetRxPending();

        Devices[i]->ProcessRx();
    }

    if (polled && pending != 0)
    {
        RxPollWork.Inc();

        /* Two polls in a row finding work, with no interrupt-driven pass
           between them, is the shape of a wakeup that is not coming. One on
           its own is just the poll winning a race against an interrupt
           already in flight. */
        if (LastPassWasPoll)
            RxStalls.Inc();
    }

    LastPassWasPoll = polled;
}

void NetDeviceTable::PollRx()
{
    if (Count == 0)
        return;

    if (!Parameters::GetInstance().IsRxPollEnabled())
        return;

    /* An interrupt has already asked; leave it to say so. */
    if (SoftIrq::GetInstance().IsPending(SoftIrq::TypeNetRx))
        return;

    RxPolls.Inc();
    PollPending.Set(1);
    SoftIrq::GetInstance().Raise(SoftIrq::TypeNetRx);
}

ulong NetDeviceTable::GetRxPolls()
{
    return RxPolls.Get();
}

ulong NetDeviceTable::GetRxPollWork()
{
    return RxPollWork.Get();
}

ulong NetDeviceTable::GetRxStalls()
{
    return RxStalls.Get();
}

void NetDeviceTable::ProcessAllTx()
{
    for (ulong i = 0; i < Count; i++)
        Devices[i]->DrainTx();
}

NetDevice* NetDeviceTable::Find(const char* name)
{
    for (ulong i = 0; i < Count; i++)
    {
        if (Devices[i] && Stdlib::StrCmp(Devices[i]->GetName(), name) == 0)
            return Devices[i];
    }
    return nullptr;
}

void NetDeviceTable::Dump(Stdlib::Printer& printer)
{
    if (Count == 0)
    {
        printer.Printf("no network devices\n");
        return;
    }

    for (ulong i = 0; i < Count; i++)
    {
        if (!Devices[i])
            continue;

        Net::MacAddress mac = Devices[i]->GetMac();
        Net::IpAddress ip = Devices[i]->GetIp();

        NetStats st;
        Stdlib::MemSet(&st, 0, sizeof(st));
        Devices[i]->GetStats(st);

        printer.Printf("%s  ", Devices[i]->GetName());
        mac.Print(printer);
        printer.Printf("  ip:");
        ip.Print(printer);
        printer.Printf("  tx:%u rx:%u drop:%u\n",
            st.TxTotal, st.RxTotal, st.RxDrop);
        printer.Printf("  rx  icmp:%u udp:%u tcp:%u arp:%u other:%u\n",
            st.RxIcmp, st.RxUdp, st.RxTcp, st.RxArp, st.RxOther);
        printer.Printf("  tx  icmp:%u udp:%u tcp:%u arp:%u other:%u\n",
            st.TxIcmp, st.TxUdp, st.TxTcp, st.TxArp, st.TxOther);
    }
}

ulong NetDeviceTable::GetCount()
{
    return Count;
}

}
