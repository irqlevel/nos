#include "dhcp.h"
#include "net.h"

#include <kernel/trace.h>
#include <kernel/asm.h>
#include <kernel/sched.h>
#include <kernel/time.h>
#include <mm/new.h>
#include <lib/stdlib.h>

namespace Kernel
{

using Net::EthHdr;
using Net::IpHdr;
using Net::UdpHdr;
using Net::IpAddress;
using Net::Htons;
using Net::Htonl;
using Net::Ntohs;
using Net::Ntohl;
using Net::IpChecksum;
using Net::EtherTypeIp;

DhcpClient::DhcpClient()
    : Dev(nullptr)
    , TaskPtr(nullptr)
    , Ready(false)
    , Xid(0)
    , RxBufLen(0)
    , RxBufReady(false)
{
    Result.LeaseTime = 0;
}

DhcpClient::~DhcpClient()
{
    Stop();
}

bool DhcpClient::Start(NetDevice* dev)
{
    if (!dev || TaskPtr)
        return false;

    Dev = dev;
    Ready = false;

    /* Use boot time as transaction ID */
    Xid = (u32)(GetBootTime().GetSecs() * 1000 + GetBootTime().GetUsecs() / 1000);
    if (Xid == 0)
        Xid = 0x12345678;

    TaskPtr = Mm::TAlloc<Task, Tag>("dhcp");
    if (!TaskPtr)
        return false;

    if (!TaskPtr->Start(&DhcpClient::TaskFunc, this))
    {
        TaskPtr->Put();
        TaskPtr = nullptr;
        return false;
    }

    return true;
}

void DhcpClient::Stop()
{
    if (TaskPtr)
    {
        TaskPtr->SetStopping();
        TaskPtr->Wait();
        TaskPtr->Put();
        TaskPtr = nullptr;
    }
    if (Dev)
    {
        Dev->UnregisterUdpListener(68);
        Dev = nullptr;
    }
}

bool DhcpClient::IsReady()
{
    return Ready;
}

DhcpResult DhcpClient::GetResult()
{
    return Result;
}

void DhcpClient::TaskFunc(void* ctx)
{
    DhcpClient* client = static_cast<DhcpClient*>(ctx);
    client->Run();
}

void DhcpClient::Run()
{
    auto* task = Task::GetCurrentTask();

    while (!task->IsStopping())
    {
        /* Register RX callback */
        Dev->RegisterUdpListener(68, RxCallbackFn, this);

        bool success = false;

        for (ulong retry = 0; retry < 3 && !task->IsStopping(); retry++)
        {
            Xid++;

            if (DoDiscover() && DoRequest())
            {
                success = true;
                break;
            }

            /* Wait before retry */
            Sleep(2 * Const::NanoSecsInSec);
        }

        /* Unregister callback */
        Dev->UnregisterUdpListener(68);

        if (!success)
        {
            Trace(0, "DHCP: failed to obtain lease");
            Sleep(5 * Const::NanoSecsInSec);
            continue;
        }

        /* Apply the lease */
        Dev->SetIp(Result.Ip);
        Dev->SetSubnetMask(Result.Mask);
        Dev->SetGateway(Result.Router);
        Ready = true;

        Trace(0, "DHCP: bound ip %u.%u.%u.%u mask %u.%u.%u.%u gw %u.%u.%u.%u lease %u",
            (ulong)((Result.Ip.Addr4 >> 24) & 0xFF), (ulong)((Result.Ip.Addr4 >> 16) & 0xFF),
            (ulong)((Result.Ip.Addr4 >> 8) & 0xFF), (ulong)(Result.Ip.Addr4 & 0xFF),
            (ulong)((Result.Mask.Addr4 >> 24) & 0xFF), (ulong)((Result.Mask.Addr4 >> 16) & 0xFF),
            (ulong)((Result.Mask.Addr4 >> 8) & 0xFF), (ulong)(Result.Mask.Addr4 & 0xFF),
            (ulong)((Result.Router.Addr4 >> 24) & 0xFF), (ulong)((Result.Router.Addr4 >> 16) & 0xFF),
            (ulong)((Result.Router.Addr4 >> 8) & 0xFF), (ulong)(Result.Router.Addr4 & 0xFF),
            (ulong)Result.LeaseTime);

        /* Sleep until T1 (half the lease time) for renewal */
        ulong t1Secs = Result.LeaseTime / 2;
        if (t1Secs < 10)
            t1Secs = 10;

        /* Sleep in 1-second intervals, checking IsStopping */
        for (ulong s = 0; s < t1Secs && !task->IsStopping(); s++)
        {
            Sleep(Const::NanoSecsInSec);
        }

        if (task->IsStopping())
            break;

        /* Renew: send REQUEST directly to server */
        Trace(0, "DHCP: renewing lease");

        Dev->RegisterUdpListener(68, RxCallbackFn, this);
        Xid++;

        bool renewed = DoRequest();

        Dev->UnregisterUdpListener(68);

        if (renewed)
        {
            Dev->SetIp(Result.Ip);
            Trace(0, "DHCP: lease renewed, lease %u", Result.LeaseTime);
        }
        else
        {
            Trace(0, "DHCP: renewal failed, restarting");
            Ready = false;
        }
    }
}

bool DhcpClient::DoDiscover()
{
    u8 frame[600];
    ulong len = BuildDiscover(frame, sizeof(frame));
    if (len == 0)
        return false;

    Dev->SendRaw(frame, len);

    /* Wait for OFFER */
    if (!WaitForResponse(DhcpOffer, 3000))
        return false;

    return true;
}

bool DhcpClient::DoRequest()
{
    u8 frame[600];
    ulong len = BuildRequest(frame, sizeof(frame));
    if (len == 0)
        return false;

    Dev->SendRaw(frame, len);

    /* Wait for ACK */
    if (!WaitForResponse(DhcpAck, 3000))
        return false;

    return true;
}

bool DhcpClient::WaitForResponse(u8 expectedType, ulong timeoutMs)
{
    {
        Stdlib::AutoLock lock(RxLock);
        RxBufReady = false;
    }

    ulong deadline = timeoutMs;
    while (deadline > 0)
    {
        /* Copy RX data under lock */
        bool ready = false;
        u8 localBuf[RxBufMaxLen];
        ulong localLen = 0;

        {
            Stdlib::AutoLock lock(RxLock);
            if (RxBufReady)
            {
                ready = true;
                localLen = RxBufLen;
                Stdlib::MemCpy(localBuf, RxBuf, localLen);
                RxBufReady = false;
            }
        }

        if (ready)
        {
            if (ParseResponse(localBuf, localLen, expectedType))
                return true;
        }

        /* Sleep 10ms */
        Sleep(10 * Const::NanoSecsInMs);
        if (deadline > 10)
            deadline -= 10;
        else
            deadline = 0;
    }

    return false;
}

ulong DhcpClient::BuildDiscover(u8* frame, ulong maxLen)
{
    ulong dhcpOptLen = 9; /* type(3) + param_request(6) + end(1) */
    ulong dhcpLen = sizeof(DhcpPacket) + 4 + dhcpOptLen; /* +4 for magic cookie */
    ulong udpLen = sizeof(UdpHdr) + dhcpLen;
    ulong ipLen = sizeof(IpHdr) + udpLen;
    ulong totalLen = sizeof(EthHdr) + ipLen;

    if (totalLen > maxLen)
        return 0;

    Stdlib::MemSet(frame, 0, totalLen);

    ulong off = 0;

    /* Ethernet header -- broadcast */
    EthHdr* eth = (EthHdr*)(frame + off);
    Stdlib::MemSet(eth->DstMac, 0xFF, 6);
    Dev->GetMac().CopyTo(eth->SrcMac);
    eth->EtherType = Htons(EtherTypeIp);
    off += sizeof(EthHdr);

    /* IP header: 0.0.0.0:68 -> 255.255.255.255:67 */
    IpHdr* ip = (IpHdr*)(frame + off);
    ip->VersionIhl = 0x45;
    ip->TotalLen = Htons((u16)ipLen);
    ip->Ttl = 128;
    ip->Protocol = Net::IpProtoUdp;
    ip->SrcAddr = 0;
    ip->DstAddr = 0xFFFFFFFF;
    ip->Checksum = Htons(IpChecksum(ip, sizeof(IpHdr)));
    off += sizeof(IpHdr);

    /* UDP header */
    UdpHdr* udp = (UdpHdr*)(frame + off);
    udp->SrcPort = Htons(68);
    udp->DstPort = Htons(67);
    udp->Length = Htons((u16)udpLen);
    udp->Checksum = 0;
    off += sizeof(UdpHdr);

    /* DHCP packet */
    DhcpPacket* dhcp = (DhcpPacket*)(frame + off);
    dhcp->Op = 1; /* BOOTREQUEST */
    dhcp->HType = 1; /* Ethernet */
    dhcp->HLen = 6;
    dhcp->Xid = Htonl(Xid);
    dhcp->Flags = Htons(0x8000); /* Broadcast */
    Dev->GetMac().CopyTo(dhcp->CHAddr);
    off += sizeof(DhcpPacket);

    /* Magic cookie */
    u8* opts = frame + off;
    opts[0] = (DhcpMagicCookie >> 24) & 0xFF;
    opts[1] = (DhcpMagicCookie >> 16) & 0xFF;
    opts[2] = (DhcpMagicCookie >> 8) & 0xFF;
    opts[3] = DhcpMagicCookie & 0xFF;
    off += 4;

    /* Option 53: DHCP Message Type = DISCOVER */
    frame[off++] = DhcpOptMessageType;
    frame[off++] = 1;
    frame[off++] = DhcpDiscover;

    /* Option 55: Parameter Request List */
    frame[off++] = DhcpOptParamRequest;
    frame[off++] = 3;
    frame[off++] = DhcpOptSubnetMask;
    frame[off++] = DhcpOptRouter;
    frame[off++] = DhcpOptDns;

    /* End */
    frame[off++] = DhcpOptEnd;

    return off;
}

ulong DhcpClient::BuildRequest(u8* frame, ulong maxLen)
{
    ulong dhcpOptLen = 3 + 6 + 6 + 1; /* type(3) + requested_ip(6) + server_id(6) + end(1) */
    ulong dhcpLen = sizeof(DhcpPacket) + 4 + dhcpOptLen;
    ulong udpLen = sizeof(UdpHdr) + dhcpLen;
    ulong ipLen = sizeof(IpHdr) + udpLen;
    ulong totalLen = sizeof(EthHdr) + ipLen;

    if (totalLen > maxLen)
        return 0;

    Stdlib::MemSet(frame, 0, totalLen);

    ulong off = 0;

    /* Ethernet header -- broadcast */
    EthHdr* eth = (EthHdr*)(frame + off);
    Stdlib::MemSet(eth->DstMac, 0xFF, 6);
    Dev->GetMac().CopyTo(eth->SrcMac);
    eth->EtherType = Htons(EtherTypeIp);
    off += sizeof(EthHdr);

    /* IP header */
    IpHdr* ip = (IpHdr*)(frame + off);
    ip->VersionIhl = 0x45;
    ip->TotalLen = Htons((u16)ipLen);
    ip->Ttl = 128;
    ip->Protocol = Net::IpProtoUdp;
    ip->SrcAddr = 0;
    ip->DstAddr = 0xFFFFFFFF;
    ip->Checksum = Htons(IpChecksum(ip, sizeof(IpHdr)));
    off += sizeof(IpHdr);

    /* UDP header */
    UdpHdr* udp = (UdpHdr*)(frame + off);
    udp->SrcPort = Htons(68);
    udp->DstPort = Htons(67);
    udp->Length = Htons((u16)udpLen);
    udp->Checksum = 0;
    off += sizeof(UdpHdr);

    /* DHCP packet */
    DhcpPacket* dhcp = (DhcpPacket*)(frame + off);
    dhcp->Op = 1;
    dhcp->HType = 1;
    dhcp->HLen = 6;
    dhcp->Xid = Htonl(Xid);
    dhcp->Flags = Htons(0x8000);
    Dev->GetMac().CopyTo(dhcp->CHAddr);
    off += sizeof(DhcpPacket);

    /* Magic cookie */
    u8* opts = frame + off;
    opts[0] = (DhcpMagicCookie >> 24) & 0xFF;
    opts[1] = (DhcpMagicCookie >> 16) & 0xFF;
    opts[2] = (DhcpMagicCookie >> 8) & 0xFF;
    opts[3] = DhcpMagicCookie & 0xFF;
    off += 4;

    /* Option 53: DHCP Message Type = REQUEST */
    frame[off++] = DhcpOptMessageType;
    frame[off++] = 1;
    frame[off++] = DhcpRequest;

    /* Option 50: Requested IP */
    frame[off++] = DhcpOptRequestedIp;
    frame[off++] = 4;
    frame[off++] = (u8)((OfferedIp.Addr4 >> 24) & 0xFF);
    frame[off++] = (u8)((OfferedIp.Addr4 >> 16) & 0xFF);
    frame[off++] = (u8)((OfferedIp.Addr4 >> 8) & 0xFF);
    frame[off++] = (u8)(OfferedIp.Addr4 & 0xFF);

    /* Option 54: Server Identifier */
    frame[off++] = DhcpOptServerId;
    frame[off++] = 4;
    frame[off++] = (u8)((ServerId.Addr4 >> 24) & 0xFF);
    frame[off++] = (u8)((ServerId.Addr4 >> 16) & 0xFF);
    frame[off++] = (u8)((ServerId.Addr4 >> 8) & 0xFF);
    frame[off++] = (u8)(ServerId.Addr4 & 0xFF);

    /* End */
    frame[off++] = DhcpOptEnd;

    return off;
}

bool DhcpClient::ParseResponse(const u8* frame, ulong len, u8 expectedType)
{
    /* frame = Ethernet + IP + UDP + DHCP */
    ulong hdrLen = sizeof(EthHdr) + sizeof(IpHdr) + sizeof(UdpHdr);
    if (len < hdrLen + sizeof(DhcpPacket) + 4)
        return false;

    const DhcpPacket* dhcp = (const DhcpPacket*)(frame + hdrLen);

    /* Check transaction ID */
    if (Ntohl(dhcp->Xid) != Xid)
        return false;

    /* Check our MAC */
    Net::MacAddress mac = Dev->GetMac();
    if (Stdlib::MemCmp(dhcp->CHAddr, mac.Bytes, 6) != 0)
        return false;

    /* Check op = BOOTREPLY */
    if (dhcp->Op != 2)
        return false;

    OfferedIp = IpAddress::FromNetwork(dhcp->YIAddr);

    /* Parse options (after magic cookie) */
    const u8* opts = frame + hdrLen + sizeof(DhcpPacket);
    ulong optsLen = len - hdrLen - sizeof(DhcpPacket);

    if (optsLen < 4)
        return false;

    /* Verify magic cookie */
    u32 cookie = ((u32)opts[0] << 24) | ((u32)opts[1] << 16) |
                 ((u32)opts[2] << 8) | (u32)opts[3];
    if (cookie != DhcpMagicCookie)
        return false;

    opts += 4;
    optsLen -= 4;

    u8 msgType = 0;
    IpAddress mask;
    IpAddress router;
    IpAddress dns;
    IpAddress serverId;
    u32 leaseTime = 0;

    ulong i = 0;
    while (i < optsLen)
    {
        u8 optCode = opts[i];
        if (optCode == DhcpOptEnd)
            break;
        if (optCode == 0) /* Padding */
        {
            i++;
            continue;
        }

        if (i + 1 >= optsLen)
            break;
        u8 optLen = opts[i + 1];
        if (i + 2 + optLen > optsLen)
            break;

        const u8* optData = &opts[i + 2];

        if (optCode == DhcpOptMessageType && optLen >= 1)
        {
            msgType = optData[0];
        }
        else if (optCode == DhcpOptSubnetMask && optLen >= 4)
        {
            mask = IpAddress(optData[0], optData[1], optData[2], optData[3]);
        }
        else if (optCode == DhcpOptRouter && optLen >= 4)
        {
            router = IpAddress(optData[0], optData[1], optData[2], optData[3]);
        }
        else if (optCode == DhcpOptDns && optLen >= 4)
        {
            dns = IpAddress(optData[0], optData[1], optData[2], optData[3]);
        }
        else if (optCode == DhcpOptServerId && optLen >= 4)
        {
            serverId = IpAddress(optData[0], optData[1], optData[2], optData[3]);
        }
        else if (optCode == DhcpOptLeaseTime && optLen >= 4)
        {
            leaseTime = ((u32)optData[0] << 24) | ((u32)optData[1] << 16) |
                        ((u32)optData[2] << 8) | (u32)optData[3];
        }

        i += 2 + optLen;
    }

    if (msgType != expectedType)
        return false;

    ServerId = serverId;
    Result.Ip = OfferedIp;
    Result.Mask = mask;
    Result.Router = router;
    Result.Dns = dns;
    Result.ServerIp = serverId;
    Result.LeaseTime = leaseTime;
    return true;
}

void DhcpClient::RxCallbackFn(const u8* frame, ulong len, void* ctx)
{
    DhcpClient* client = static_cast<DhcpClient*>(ctx);

    Stdlib::AutoLock lock(client->RxLock);

    if (client->RxBufReady)
        return; /* Previous response not consumed yet */

    if (len > RxBufMaxLen)
        len = RxBufMaxLen;

    Stdlib::MemCpy(client->RxBuf, frame, len);
    client->RxBufLen = len;
    client->RxBufReady = true;
}

}
