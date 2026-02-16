#include "net_device.h"

#include <kernel/trace.h>
#include <lib/stdlib.h>
#include <mm/new.h>

namespace Kernel
{

NetDevice::NetDevice()
    : TxCount(0)
    , RxCount(0)
    , UdpListenerCount(0)
{
    Stdlib::MemSet(UdpListeners, 0, sizeof(UdpListeners));
}

bool NetDevice::SubmitTx(NetFrame* frame)
{
    ulong flags = TxQueueLock.LockIrqSave();
    if (TxCount >= TxQueueCapacity)
    {
        TxQueueLock.UnlockIrqRestore(flags);
        frame->Put();
        return false;
    }
    TxQueue.InsertTail(&frame->Link);
    TxCount++;
    FlushTx();
    TxQueueLock.UnlockIrqRestore(flags);
    return true;
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
    Stdlib::AutoLock lock(UdpListenerLock);

    for (ulong i = 0; i < UdpListenerCount; i++)
    {
        if (UdpListeners[i].Port == port)
        {
            for (ulong j = i; j + 1 < UdpListenerCount; j++)
                UdpListeners[j] = UdpListeners[j + 1];
            UdpListenerCount--;
            Stdlib::MemSet(&UdpListeners[UdpListenerCount], 0, sizeof(UdpListener));
            return;
        }
    }
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

NetDeviceTable::NetDeviceTable()
    : Count(0)
{
    for (ulong i = 0; i < MaxDevices; i++)
        Devices[i] = nullptr;
}

NetDeviceTable::~NetDeviceTable()
{
}

bool NetDeviceTable::Register(NetDevice* dev)
{
    if (Count >= MaxDevices || dev == nullptr)
        return false;

    Devices[Count] = dev;
    Count++;

    Net::MacAddress mac = dev->GetMac();

    Trace(0, "NetDevice registered: %s mac %p:%p:%p:%p:%p:%p",
        dev->GetName(),
        (ulong)mac.Bytes[0], (ulong)mac.Bytes[1], (ulong)mac.Bytes[2],
        (ulong)mac.Bytes[3], (ulong)mac.Bytes[4], (ulong)mac.Bytes[5]);

    return true;
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
