#include "icmp.h"
#include "net.h"
#include "arp.h"

#include <kernel/trace.h>
#include <kernel/time.h>
#include <kernel/sched.h>
#include <lib/stdlib.h>
#include <include/const.h>

namespace Kernel
{

using Net::EthHdr;
using Net::IpHdr;
using Net::IcmpHdr;
using Net::MacAddress;
using Net::IpAddress;
using Net::Htons;
using Net::Htonl;
using Net::Ntohs;
using Net::Ntohl;
using Net::IpChecksum;
using Net::EtherTypeIp;

Icmp::Icmp()
{
    Reply.Valid = false;
    Reply.Id = 0;
    Reply.Seq = 0;
}

Icmp::~Icmp()
{
}

void Icmp::Process(NetDevice* dev, const u8* frame, ulong len)
{
    if (len < sizeof(EthHdr) + sizeof(IpHdr) + sizeof(IcmpHdr))
    {
        RxTooShort.Inc();
        return;
    }

    const EthHdr* eth = (const EthHdr*)frame;
    const IpHdr* ip = (const IpHdr*)(frame + sizeof(EthHdr));
    const IcmpHdr* icmp = (const IcmpHdr*)(frame + sizeof(EthHdr) + sizeof(IpHdr));

    if (icmp->Type == TypeEchoRequest && icmp->Code == 0)
    {
        EchoReqRx.Inc();
        Trace(0, "ICMP echo request: srcIp %p dstIp %p id %u seq %u",
            (ulong)Ntohl(ip->SrcAddr), (ulong)Ntohl(ip->DstAddr),
            (ulong)Ntohs(icmp->Id), (ulong)Ntohs(icmp->Seq));
        /* Build echo reply */
        ulong ipTotalLen = Ntohs(ip->TotalLen);
        if (ipTotalLen < sizeof(IpHdr) + sizeof(IcmpHdr))
            return;
        if (sizeof(EthHdr) + ipTotalLen > len)
            return;

        ulong icmpLen = ipTotalLen - sizeof(IpHdr);
        ulong replyFrameLen = sizeof(EthHdr) + sizeof(IpHdr) + icmpLen;

        if (replyFrameLen > 1514)
            return;

        u8 reply[1514];
        Stdlib::MemSet(reply, 0, sizeof(reply));

        /* Ethernet header -- swap src/dst */
        EthHdr* rEth = (EthHdr*)reply;
        Stdlib::MemCpy(rEth->DstMac, eth->SrcMac, 6);
        dev->GetMac().CopyTo(rEth->SrcMac);
        rEth->EtherType = Htons(EtherTypeIp);

        /* IP header -- swap src/dst, recalculate checksum */
        IpHdr* rIp = (IpHdr*)(reply + sizeof(EthHdr));
        rIp->VersionIhl = 0x45;
        rIp->Tos = 0;
        rIp->TotalLen = Htons((u16)(sizeof(IpHdr) + icmpLen));
        rIp->Id = 0;
        rIp->FragOff = 0;
        rIp->Ttl = 64;
        rIp->Protocol = Net::IpProtoIcmp;
        rIp->Checksum = 0;
        rIp->SrcAddr = ip->DstAddr;
        rIp->DstAddr = ip->SrcAddr;
        rIp->Checksum = Htons(IpChecksum(rIp, sizeof(IpHdr)));

        /* ICMP -- copy entire ICMP payload, change type to reply */
        u8* rIcmpRaw = reply + sizeof(EthHdr) + sizeof(IpHdr);
        const u8* srcIcmpRaw = frame + sizeof(EthHdr) + sizeof(IpHdr);
        Stdlib::MemCpy(rIcmpRaw, srcIcmpRaw, icmpLen);

        IcmpHdr* rIcmp = (IcmpHdr*)rIcmpRaw;
        rIcmp->Type = TypeEchoReply;
        rIcmp->Code = 0;
        rIcmp->Checksum = 0;
        rIcmp->Checksum = Htons(IpChecksum(rIcmpRaw, icmpLen));

        Trace(0, "ICMP echo reply: srcIp %p dstIp %p len %u",
            (ulong)Ntohl(rIp->SrcAddr), (ulong)Ntohl(rIp->DstAddr), replyFrameLen);
        Trace(0, "ICMP echo reply: dstMac %p:%p:%p:%p:%p:%p srcMac %p:%p:%p:%p:%p:%p",
            (ulong)rEth->DstMac[0], (ulong)rEth->DstMac[1], (ulong)rEth->DstMac[2],
            (ulong)rEth->DstMac[3], (ulong)rEth->DstMac[4], (ulong)rEth->DstMac[5],
            (ulong)rEth->SrcMac[0], (ulong)rEth->SrcMac[1], (ulong)rEth->SrcMac[2],
            (ulong)rEth->SrcMac[3], (ulong)rEth->SrcMac[4], (ulong)rEth->SrcMac[5]);

        if (dev->SendRaw(reply, replyFrameLen))
            EchoReplyTx.Inc();
        else
            EchoReplyTxFail.Inc();
    }
    else if (icmp->Type == TypeEchoReply && icmp->Code == 0)
    {
        EchoReplyRx.Inc();
        /* Store reply for WaitReply() */
        Stdlib::AutoLock lock(Lock);
        Reply.Valid = true;
        Reply.Id = Ntohs(icmp->Id);
        Reply.Seq = Ntohs(icmp->Seq);
        Reply.Timestamp = GetBootTime();
    }
    else
    {
        RxOther.Inc();
    }
}

bool Icmp::SendEchoRequest(NetDevice* dev, IpAddress dstIp, u16 id, u16 seq)
{
    /* Resolve destination MAC via ARP */
    MacAddress dstMac;
    if (!ArpTable::GetInstance().Resolve(dev, dstIp, dstMac))
    {
        dstMac = MacAddress::Broadcast();
    }

    static const ulong PayloadSize = 32;
    ulong icmpLen = sizeof(IcmpHdr) + PayloadSize;
    ulong ipLen = sizeof(IpHdr) + icmpLen;
    ulong frameLen = sizeof(EthHdr) + ipLen;

    u8 frame[1514];
    Stdlib::MemSet(frame, 0, sizeof(frame));

    ulong off = 0;

    /* Ethernet header */
    EthHdr* eth = (EthHdr*)(frame + off);
    dstMac.CopyTo(eth->DstMac);
    dev->GetMac().CopyTo(eth->SrcMac);
    eth->EtherType = Htons(EtherTypeIp);
    off += sizeof(EthHdr);

    /* IP header */
    IpHdr* ip = (IpHdr*)(frame + off);
    ip->VersionIhl = 0x45;
    ip->TotalLen = Htons((u16)ipLen);
    ip->Ttl = 64;
    ip->Protocol = Net::IpProtoIcmp;
    ip->SrcAddr = dev->GetIp().ToNetwork();
    ip->DstAddr = dstIp.ToNetwork();
    ip->Checksum = Htons(IpChecksum(ip, sizeof(IpHdr)));
    off += sizeof(IpHdr);

    /* ICMP echo request */
    IcmpHdr* icmp = (IcmpHdr*)(frame + off);
    icmp->Type = TypeEchoRequest;
    icmp->Code = 0;
    icmp->Id = Htons(id);
    icmp->Seq = Htons(seq);
    off += sizeof(IcmpHdr);

    /* Payload -- fill with pattern */
    for (ulong i = 0; i < PayloadSize; i++)
        frame[off + i] = (u8)(i & 0xFF);
    off += PayloadSize;

    /* Compute ICMP checksum over header + payload */
    icmp->Checksum = 0;
    icmp->Checksum = Htons(IpChecksum(icmp, icmpLen));

    /* Clear reply slot and record send time */
    {
        Stdlib::AutoLock lock(Lock);
        Reply.Valid = false;
        SendTime = GetBootTime();
    }

    bool ok = dev->SendRaw(frame, frameLen);
    if (ok)
        EchoReqTx.Inc();
    return ok;
}

bool Icmp::WaitReply(u16 id, u16 seq, ulong timeoutMs, ulong& rttNs)
{
    Stdlib::Time deadline = GetBootTime() + Stdlib::Time(timeoutMs * Const::NanoSecsInMs);

    while (GetBootTime() < deadline)
    {
        {
            Stdlib::AutoLock lock(Lock);
            if (Reply.Valid && Reply.Id == id && Reply.Seq == seq)
            {
                Stdlib::Time rtt = Reply.Timestamp - SendTime;
                rttNs = rtt.GetValue();
                Reply.Valid = false;
                return true;
            }
        }

        Sleep(10 * Const::NanoSecsInMs);
    }

    return false;
}

void Icmp::Dump(Stdlib::Printer& printer)
{
    printer.Printf("echo request  rx:%u tx:%u\n",
        EchoReqRx.Get(), EchoReqTx.Get());
    printer.Printf("echo reply    rx:%u tx:%u tx-fail:%u\n",
        EchoReplyRx.Get(), EchoReplyTx.Get(), EchoReplyTxFail.Get());
    printer.Printf("other         rx:%u short:%u\n",
        RxOther.Get(), RxTooShort.Get());
}

}
