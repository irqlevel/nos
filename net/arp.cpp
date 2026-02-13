#include "arp.h"
#include "net.h"

#include <kernel/trace.h>
#include <kernel/asm.h>
#include <kernel/sched.h>
#include <lib/stdlib.h>
#include <include/const.h>

namespace Kernel
{

using Net::EthHdr;
using Net::ArpPacket;
using Net::MacAddress;
using Net::IpAddress;
using Net::Htons;
using Net::Htonl;
using Net::Ntohs;
using Net::Ntohl;
using Net::EtherTypeArp;

ArpTable::ArpTable()
{
    Stdlib::MemSet(Cache, 0, sizeof(Cache));
}

ArpTable::~ArpTable()
{
}

bool ArpTable::Lookup(IpAddress ip, MacAddress& mac)
{
    Stdlib::AutoLock lock(Lock);
    for (ulong i = 0; i < CacheSize; i++)
    {
        if (Cache[i].Valid && Cache[i].Ip == ip)
        {
            mac = Cache[i].Mac;
            return true;
        }
    }
    return false;
}

void ArpTable::Insert(IpAddress ip, const MacAddress& mac)
{
    Stdlib::AutoLock lock(Lock);

    /* Look for existing entry first */
    for (ulong i = 0; i < CacheSize; i++)
    {
        if (Cache[i].Valid && Cache[i].Ip == ip)
        {
            Cache[i].Mac = mac;
            return;
        }
    }

    /* Find an empty slot */
    for (ulong i = 0; i < CacheSize; i++)
    {
        if (!Cache[i].Valid)
        {
            Cache[i].Ip = ip;
            Cache[i].Mac = mac;
            Cache[i].Valid = true;
            return;
        }
    }

    /* Cache full, overwrite first entry */
    Cache[0].Ip = ip;
    Cache[0].Mac = mac;
    Cache[0].Valid = true;
}

void ArpTable::Process(NetDevice* dev, const u8* frame, ulong len)
{
    if (len < sizeof(EthHdr) + sizeof(ArpPacket))
        return;

    const ArpPacket* arp = (const ArpPacket*)(frame + sizeof(EthHdr));

    u16 opcode = Ntohs(arp->Opcode);
    bool needReply = false;

    if (opcode == 1) /* ARP Request */
    {
        if (IpAddress::FromNetwork(arp->TargetIp) == dev->GetIp())
            needReply = true;

        /* Learn the sender's MAC (Insert acquires lock internally) */
        Insert(IpAddress::FromNetwork(arp->SenderIp), MacAddress(arp->SenderMac));
    }
    else if (opcode == 2) /* ARP Reply */
    {
        Insert(IpAddress::FromNetwork(arp->SenderIp), MacAddress(arp->SenderMac));
    }

    /* Send reply outside any lock */
    if (needReply)
        SendReply(dev, frame);
}

void ArpTable::SendReply(NetDevice* dev, const u8* reqFrame)
{
    const EthHdr* reqEth = (const EthHdr*)reqFrame;
    const ArpPacket* reqArp = (const ArpPacket*)(reqFrame + sizeof(EthHdr));

    u8 frame[sizeof(EthHdr) + sizeof(ArpPacket)];
    Stdlib::MemSet(frame, 0, sizeof(frame));

    /* Ethernet header */
    EthHdr* eth = (EthHdr*)frame;
    Stdlib::MemCpy(eth->DstMac, reqEth->SrcMac, 6);
    dev->GetMac().CopyTo(eth->SrcMac);
    eth->EtherType = Htons(EtherTypeArp);

    /* ARP reply */
    ArpPacket* arp = (ArpPacket*)(frame + sizeof(EthHdr));
    arp->HwType = Htons(1);
    arp->ProtoType = Htons(0x0800);
    arp->HwSize = 6;
    arp->ProtoSize = 4;
    arp->Opcode = Htons(2); /* Reply */
    dev->GetMac().CopyTo(arp->SenderMac);
    arp->SenderIp = dev->GetIp().ToNetwork();
    Stdlib::MemCpy(arp->TargetMac, reqArp->SenderMac, 6);
    arp->TargetIp = reqArp->SenderIp;

    dev->SendRaw(frame, sizeof(frame));
}

void ArpTable::SendRequest(NetDevice* dev, IpAddress ip)
{
    u8 frame[sizeof(EthHdr) + sizeof(ArpPacket)];
    Stdlib::MemSet(frame, 0, sizeof(frame));

    /* Ethernet header -- broadcast */
    EthHdr* eth = (EthHdr*)frame;
    Stdlib::MemSet(eth->DstMac, 0xFF, 6);
    dev->GetMac().CopyTo(eth->SrcMac);
    eth->EtherType = Htons(EtherTypeArp);

    /* ARP request */
    ArpPacket* arp = (ArpPacket*)(frame + sizeof(EthHdr));
    arp->HwType = Htons(1);
    arp->ProtoType = Htons(0x0800);
    arp->HwSize = 6;
    arp->ProtoSize = 4;
    arp->Opcode = Htons(1); /* Request */
    dev->GetMac().CopyTo(arp->SenderMac);
    arp->SenderIp = dev->GetIp().ToNetwork();
    Stdlib::MemSet(arp->TargetMac, 0, 6);
    arp->TargetIp = ip.ToNetwork();

    dev->SendRaw(frame, sizeof(frame));
}

void ArpTable::Dump(Stdlib::Printer& printer)
{
    Stdlib::AutoLock lock(Lock);

    bool any = false;
    for (ulong i = 0; i < CacheSize; i++)
    {
        if (!Cache[i].Valid)
            continue;

        Cache[i].Ip.Print(printer);
        printer.Printf("  ");
        Cache[i].Mac.Print(printer);
        printer.Printf("\n");
        any = true;
    }

    if (!any)
        printer.Printf("arp table empty\n");
}

bool ArpTable::Resolve(NetDevice* dev, IpAddress ip, MacAddress& mac)
{
    /* Check cache first */
    if (Lookup(ip, mac))
        return true;

    /* Send request and poll for reply.
       DrainRx now runs in the soft IRQ task, so we Sleep to let it process. */
    SendRequest(dev, ip);

    for (ulong attempt = 0; attempt < 3000; attempt++)
    {
        if (Lookup(ip, mac))
            return true;

        Sleep(1 * Const::NanoSecsInMs);
    }

    return false;
}

}
