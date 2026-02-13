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

bool ArpTable::Lookup(u32 ip, u8 mac[6])
{
    Stdlib::AutoLock lock(Lock);
    for (ulong i = 0; i < CacheSize; i++)
    {
        if (Cache[i].Valid && Cache[i].Ip == ip)
        {
            Stdlib::MemCpy(mac, Cache[i].Mac, 6);
            return true;
        }
    }
    return false;
}

void ArpTable::Insert(u32 ip, const u8 mac[6])
{
    Stdlib::AutoLock lock(Lock);

    /* Look for existing entry first */
    for (ulong i = 0; i < CacheSize; i++)
    {
        if (Cache[i].Valid && Cache[i].Ip == ip)
        {
            Stdlib::MemCpy(Cache[i].Mac, mac, 6);
            return;
        }
    }

    /* Find an empty slot */
    for (ulong i = 0; i < CacheSize; i++)
    {
        if (!Cache[i].Valid)
        {
            Cache[i].Ip = ip;
            Stdlib::MemCpy(Cache[i].Mac, mac, 6);
            Cache[i].Valid = true;
            return;
        }
    }

    /* Cache full, overwrite first entry */
    Cache[0].Ip = ip;
    Stdlib::MemCpy(Cache[0].Mac, mac, 6);
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
        if (Ntohl(arp->TargetIp) == dev->GetIp())
            needReply = true;

        /* Learn the sender's MAC (Insert acquires lock internally) */
        Insert(Ntohl(arp->SenderIp), arp->SenderMac);
    }
    else if (opcode == 2) /* ARP Reply */
    {
        Insert(Ntohl(arp->SenderIp), arp->SenderMac);
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
    dev->GetMac(eth->SrcMac);
    eth->EtherType = Htons(EtherTypeArp);

    /* ARP reply */
    ArpPacket* arp = (ArpPacket*)(frame + sizeof(EthHdr));
    arp->HwType = Htons(1);
    arp->ProtoType = Htons(0x0800);
    arp->HwSize = 6;
    arp->ProtoSize = 4;
    arp->Opcode = Htons(2); /* Reply */
    dev->GetMac(arp->SenderMac);
    arp->SenderIp = Htonl(dev->GetIp());
    Stdlib::MemCpy(arp->TargetMac, reqArp->SenderMac, 6);
    arp->TargetIp = reqArp->SenderIp;

    dev->SendRaw(frame, sizeof(frame));
}

void ArpTable::SendRequest(NetDevice* dev, u32 ip)
{
    u8 frame[sizeof(EthHdr) + sizeof(ArpPacket)];
    Stdlib::MemSet(frame, 0, sizeof(frame));

    /* Ethernet header -- broadcast */
    EthHdr* eth = (EthHdr*)frame;
    Stdlib::MemSet(eth->DstMac, 0xFF, 6);
    dev->GetMac(eth->SrcMac);
    eth->EtherType = Htons(EtherTypeArp);

    /* ARP request */
    ArpPacket* arp = (ArpPacket*)(frame + sizeof(EthHdr));
    arp->HwType = Htons(1);
    arp->ProtoType = Htons(0x0800);
    arp->HwSize = 6;
    arp->ProtoSize = 4;
    arp->Opcode = Htons(1); /* Request */
    dev->GetMac(arp->SenderMac);
    arp->SenderIp = Htonl(dev->GetIp());
    Stdlib::MemSet(arp->TargetMac, 0, 6);
    arp->TargetIp = Htonl(ip);

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

        u32 ip = Cache[i].Ip;
        const u8* m = Cache[i].Mac;
        printer.Printf("%u.%u.%u.%u  %p:%p:%p:%p:%p:%p\n",
            (ulong)((ip >> 24) & 0xFF),
            (ulong)((ip >> 16) & 0xFF),
            (ulong)((ip >> 8) & 0xFF),
            (ulong)(ip & 0xFF),
            (ulong)m[0], (ulong)m[1], (ulong)m[2],
            (ulong)m[3], (ulong)m[4], (ulong)m[5]);
        any = true;
    }

    if (!any)
        printer.Printf("arp table empty\n");
}

bool ArpTable::Resolve(NetDevice* dev, u32 ip, u8 mac[6])
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
