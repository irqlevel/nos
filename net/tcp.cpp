#include "tcp.h"
#include <kernel/trace.h>
#include <kernel/sched.h>
#include <kernel/softirq.h>
#include <kernel/asm.h>
#include <kernel/time.h>
#include <net/arp.h>
#include <lib/stdlib.h>

namespace Kernel
{

using namespace Net;

/* --- TcpConn --- */

void TcpConn::Init()
{
    LocalIp = IpAddress();
    LocalPort = 0;
    RemoteIp = IpAddress();
    RemotePort = 0;
    Dev = nullptr;
    ResolvedMac = MacAddress();
    State = TcpStateFree;
    SndUna = 0;
    SndNxt = 0;
    SndWnd = 0;
    RcvNxt = 0;
    RcvWnd = TcpRecvBufSize;
    Iss = 0;
    Irs = 0;
    PeerMss = TcpDefaultMss;
    SendBuf.Init(TcpSendBufSize);
    RecvBuf.Init(TcpRecvBufSize);
    RtoMs = TcpInitialRtoMs;
    RetransmitDeadlineMs = 0;
    TimeWaitDeadlineMs = 0;
    DataReady.Set(0);
    ConnReady.Set(0);
    NeedCleanup = false;
    FinAcked = false;
    HashLink.Init();
}

void TcpConn::Reset()
{
    Init();
}

/* --- Tcp singleton --- */

Tcp::Tcp()
    : NextEphemeralPort(TcpEphemeralPortBase)
    , Initialized(false)
{
    for (ulong i = 0; i < TcpConnHashSize; i++)
        HashTable[i].Init();

    for (ulong i = 0; i < TcpMaxConnections; i++)
        Pool[i].Init();
}

Tcp::~Tcp()
{
}

bool Tcp::Init()
{
    if (Initialized)
        return true;

    SoftIrq::GetInstance().Register(SoftIrq::TypeTcpTimer,
                                    TcpTimerSoftIrqHandler, nullptr);

    Stdlib::Time period(TcpTimerPeriodMs * Const::NanoSecsInMs);
    if (!TimerTable::GetInstance().StartTimer(*this, period))
    {
        Trace(0, "Tcp: failed to start retransmit timer");
        return false;
    }

    Initialized = true;
    Trace(0, "Tcp: initialized, max %u connections", TcpMaxConnections);
    return true;
}

void Tcp::TcpTimerSoftIrqHandler(void* ctx)
{
    (void)ctx;
    Tcp::GetInstance().ProcessRetransmits();
}

void Tcp::OnTick(TimerCallback& callback)
{
    (void)callback;
    SoftIrq::GetInstance().Raise(SoftIrq::TypeTcpTimer);
}

ulong Tcp::GetBootTimeMs()
{
    Stdlib::Time t = GetBootTime();
    return t.GetSecs() * 1000 + t.GetUsecs() / 1000;
}

/* --- Hash table helpers --- */

ulong Tcp::HashIndex(u32 localIp, u16 localPort,
                     u32 remoteIp, u16 remotePort)
{
    u32 h = localIp ^ remoteIp ^ ((u32)localPort << 16) ^ (u32)remotePort;
    h ^= (h >> 16);
    h ^= (h >> 8);
    return h % TcpConnHashSize;
}

void Tcp::InsertHash(TcpConn* conn)
{
    ulong idx = HashIndex(conn->LocalIp.Addr4, conn->LocalPort,
                          conn->RemoteIp.Addr4, conn->RemotePort);
    HashTable[idx].InsertTail(&conn->HashLink);
}

void Tcp::RemoveHash(TcpConn* conn)
{
    conn->HashLink.RemoveInit();
}

/* Caller must hold PoolLock. Returns conn with conn->Lock held, or nullptr. */
TcpConn* Tcp::LookupLocked(u32 localIp, u16 localPort,
                            u32 remoteIp, u16 remotePort)
{
    ulong idx = HashIndex(localIp, localPort, remoteIp, remotePort);
    Stdlib::ListEntry* head = &HashTable[idx];
    Stdlib::ListEntry* entry = head->Flink;

    while (entry != head)
    {
        TcpConn* conn = CONTAINING_RECORD(entry, TcpConn, HashLink);
        entry = entry->Flink;
        if (conn->LocalIp.Addr4 == localIp &&
            conn->LocalPort == localPort &&
            conn->RemoteIp.Addr4 == remoteIp &&
            conn->RemotePort == remotePort &&
            conn->State != TcpStateFree)
        {
            conn->Lock.Lock();
            return conn;
        }
    }
    return nullptr;
}

/* Caller must hold PoolLock. Returns listener with Lock held, or nullptr. */
TcpConn* Tcp::FindListenerLocked(u16 localPort)
{
    for (ulong i = 0; i < TcpMaxConnections; i++)
    {
        TcpConn* conn = &Pool[i];
        if (conn->State == TcpStateListen && conn->LocalPort == localPort)
        {
            conn->Lock.Lock();
            if (conn->State == TcpStateListen && conn->LocalPort == localPort)
                return conn;
            conn->Lock.Unlock();
        }
    }
    return nullptr;
}

/* Caller must hold PoolLock. Returns first Free slot or nullptr. */
TcpConn* Tcp::AllocConn()
{
    for (ulong i = 0; i < TcpMaxConnections; i++)
    {
        if (Pool[i].State == TcpStateFree)
        {
            Pool[i].Init();
            return &Pool[i];
        }
    }
    return nullptr;
}

/* Caller must hold PortMutex. Scans pool under PoolLock for collisions. */
u16 Tcp::AllocEphemeralPort()
{
    u16 startPort = NextEphemeralPort;
    for (;;)
    {
        u16 port = NextEphemeralPort;
        NextEphemeralPort++;
        if (NextEphemeralPort > TcpEphemeralPortMax)
            NextEphemeralPort = TcpEphemeralPortBase;

        /* Check collision against active connections */
        bool collision = false;
        for (ulong i = 0; i < TcpMaxConnections; i++)
        {
            if (Pool[i].State != TcpStateFree && Pool[i].LocalPort == port)
            {
                collision = true;
                break;
            }
        }
        if (!collision)
            return port;

        /* Wrapped around without finding a free port */
        if (NextEphemeralPort == startPort)
            return 0;
    }
}

/* --- Send segment --- */

void Tcp::SendSegment(TcpConn* conn, u8 flags, const u8* data, ulong dataLen)
{
    bool hasMssOption = (flags & TcpFlagSyn);
    ulong tcpHdrLen = hasMssOption ? 24 : sizeof(TcpHdr);
    ulong tcpLen = tcpHdrLen + dataLen;
    ulong ipLen = sizeof(IpHdr) + tcpLen;
    ulong frameLen = sizeof(EthHdr) + ipLen;

    static const ulong MaxFrameLen = 1514;
    if (frameLen > MaxFrameLen)
        return;

    u8 frame[MaxFrameLen];
    Stdlib::MemSet(frame, 0, sizeof(frame));

    /* Ethernet header */
    EthHdr* eth = (EthHdr*)frame;
    conn->ResolvedMac.CopyTo(eth->DstMac);
    conn->Dev->GetMac().CopyTo(eth->SrcMac);
    eth->EtherType = Htons(EtherTypeIp);

    /* IP header */
    IpHdr* ip = (IpHdr*)(frame + sizeof(EthHdr));
    ip->VersionIhl = 0x45;
    ip->TotalLen = Htons((u16)ipLen);
    ip->Ttl = TcpDefaultTtl;
    ip->Protocol = IpProtoTcp;
    ip->SrcAddr = conn->LocalIp.ToNetwork();
    ip->DstAddr = conn->RemoteIp.ToNetwork();
    ip->Checksum = Htons(IpChecksum(ip, sizeof(IpHdr)));

    /* TCP header */
    TcpHdr* tcp = (TcpHdr*)(frame + sizeof(EthHdr) + sizeof(IpHdr));
    tcp->SrcPort = Htons(conn->LocalPort);
    tcp->DstPort = Htons(conn->RemotePort);
    tcp->SeqNum = Htonl(conn->SndNxt);
    tcp->AckNum = Htonl(conn->RcvNxt);
    tcp->DataOff = hasMssOption ? TcpDataOff6 : TcpDataOff5;
    tcp->Flags = flags;
    tcp->Window = Htons((u16)conn->RcvWnd);
    tcp->UrgentPtr = 0;

    /* MSS option in SYN/SYN-ACK */
    if (hasMssOption)
    {
        u8* opt = (u8*)(tcp + 1);
        opt[0] = TcpOptMss;
        opt[1] = TcpOptMssLen;
        opt[2] = (u8)(TcpOurMss >> 8);
        opt[3] = (u8)(TcpOurMss & 0xFF);
    }

    /* Payload */
    if (dataLen > 0 && data != nullptr)
        Stdlib::MemCpy(frame + sizeof(EthHdr) + sizeof(IpHdr) + tcpHdrLen,
                       data, dataLen);

    /* TCP checksum */
    tcp->Checksum = 0;
    tcp->Checksum = Htons(TcpChecksum(ip->SrcAddr, ip->DstAddr,
                                      tcp, tcpLen));

    conn->Dev->SendRaw(frame, frameLen);
    TxSegments.Inc();
}

void Tcp::SendRst(NetDevice* dev, const MacAddress& dstMac,
                  IpAddress srcIp, IpAddress dstIp,
                  u16 srcPort, u16 dstPort, u32 seq, u32 ack)
{
    static const ulong MaxFrameLen = 1514;
    ulong tcpLen = sizeof(TcpHdr);
    ulong ipLen = sizeof(IpHdr) + tcpLen;
    ulong frameLen = sizeof(EthHdr) + ipLen;

    u8 frame[MaxFrameLen];
    Stdlib::MemSet(frame, 0, sizeof(frame));

    EthHdr* eth = (EthHdr*)frame;
    dstMac.CopyTo(eth->DstMac);
    dev->GetMac().CopyTo(eth->SrcMac);
    eth->EtherType = Htons(EtherTypeIp);

    IpHdr* ip = (IpHdr*)(frame + sizeof(EthHdr));
    ip->VersionIhl = 0x45;
    ip->TotalLen = Htons((u16)ipLen);
    ip->Ttl = TcpDefaultTtl;
    ip->Protocol = IpProtoTcp;
    ip->SrcAddr = srcIp.ToNetwork();
    ip->DstAddr = dstIp.ToNetwork();
    ip->Checksum = Htons(IpChecksum(ip, sizeof(IpHdr)));

    TcpHdr* tcp = (TcpHdr*)(frame + sizeof(EthHdr) + sizeof(IpHdr));
    tcp->SrcPort = Htons(srcPort);
    tcp->DstPort = Htons(dstPort);
    tcp->SeqNum = Htonl(seq);
    tcp->AckNum = Htonl(ack);
    tcp->DataOff = TcpDataOff5;
    tcp->Flags = TcpFlagRst | TcpFlagAck;
    tcp->Window = 0;

    tcp->Checksum = 0;
    tcp->Checksum = Htons(TcpChecksum(ip->SrcAddr, ip->DstAddr,
                                      tcp, tcpLen));

    dev->SendRaw(frame, frameLen);
    TxSegments.Inc();
}

/* --- Parse MSS option from TCP header --- */

static u16 ParseMssOption(const TcpHdr* tcp)
{
    ulong hdrLen = ((tcp->DataOff >> 4) & 0xF) * 4;
    if (hdrLen <= sizeof(TcpHdr))
        return TcpDefaultMss;

    const u8* opts = (const u8*)tcp + sizeof(TcpHdr);
    ulong optsLen = hdrLen - sizeof(TcpHdr);
    ulong i = 0;

    while (i < optsLen)
    {
        u8 kind = opts[i];
        if (kind == TcpOptEnd)
            break;
        if (kind == TcpOptNop)
        {
            i++;
            continue;
        }
        if (i + 1 >= optsLen)
            break;
        u8 optLen = opts[i + 1];
        if (optLen < 2 || i + optLen > optsLen)
            break;
        if (kind == TcpOptMss && optLen == TcpOptMssLen)
        {
            u16 mss = (u16)((opts[i + 2] << 8) | opts[i + 3]);
            if (mss > TcpOurMss)
                mss = TcpOurMss;
            if (mss == 0)
                mss = TcpDefaultMss;
            return mss;
        }
        i += optLen;
    }
    return TcpDefaultMss;
}

/* --- State machine --- */

void Tcp::HandleState(TcpConn* conn, const IpHdr* ip,
                      const TcpHdr* tcp, const u8* payload,
                      ulong payloadLen)
{
    (void)ip;
    u32 seq = Ntohl(tcp->SeqNum);
    u32 ack = Ntohl(tcp->AckNum);
    u8 flags = tcp->Flags;
    u16 wnd = Ntohs(tcp->Window);
    ulong now = GetBootTimeMs();

    /* RST handling -- valid in all states except Free/Listen */
    if ((flags & TcpFlagRst) && conn->State != TcpStateListen)
    {
        Trace(0, "Tcp: RST received, conn %u:%u -> %u:%u",
              (ulong)conn->LocalPort, (ulong)conn->RemotePort,
              (ulong)Ntohs(tcp->SrcPort), (ulong)Ntohs(tcp->DstPort));
        conn->State = TcpStateClosed;
        conn->ConnReady.Set(1);
        conn->DataReady.Set(1);
        return;
    }

    switch (conn->State)
    {
    case TcpStateSynSent:
    {
        /* Expecting SYN+ACK */
        if ((flags & (TcpFlagSyn | TcpFlagAck)) == (TcpFlagSyn | TcpFlagAck))
        {
            if (ack != conn->SndNxt)
            {
                /* Bad ACK -- send RST */
                SendRst(conn->Dev, conn->ResolvedMac,
                        conn->LocalIp, conn->RemoteIp,
                        conn->LocalPort, conn->RemotePort,
                        ack, 0);
                return;
            }
            conn->Irs = seq;
            conn->RcvNxt = seq + 1;
            conn->SndUna = ack;
            conn->SndWnd = wnd;
            conn->PeerMss = ParseMssOption(tcp);
            conn->State = TcpStateEstablished;
            conn->RtoMs = TcpInitialRtoMs;
            conn->RetransmitDeadlineMs = 0;

            /* Send ACK */
            u32 savedNxt = conn->SndNxt;
            SendSegment(conn, TcpFlagAck, nullptr, 0);
            conn->SndNxt = savedNxt; /* ACK doesn't consume seq */

            conn->ConnReady.Set(1);
            Trace(0, "Tcp: connected %u -> %u, mss %u",
                  (ulong)conn->LocalPort, (ulong)conn->RemotePort,
                  (ulong)conn->PeerMss);
        }
        break;
    }
    case TcpStateSynReceived:
    {
        /* Expecting ACK to complete handshake */
        if ((flags & TcpFlagAck) && ack == conn->SndNxt)
        {
            conn->SndUna = ack;
            conn->SndWnd = wnd;
            conn->State = TcpStateEstablished;
            conn->RtoMs = TcpInitialRtoMs;
            conn->RetransmitDeadlineMs = 0;
            conn->ConnReady.Set(1);
            Trace(0, "Tcp: accepted %u <- %u",
                  (ulong)conn->LocalPort, (ulong)conn->RemotePort);
        }
        break;
    }
    case TcpStateEstablished:
    {
        /* ACK processing */
        if (flags & TcpFlagAck)
        {
            /* Advance SndUna if ACK is in range */
            long ackAdvance = (long)(ack - conn->SndUna);
            if (ackAdvance > 0 && ack <= conn->SndNxt) /* valid forward ACK, note: wrapping not handled for simplicity */
            {
                conn->SendBuf.Consume((ulong)ackAdvance);
                conn->SndUna = ack;
                conn->SndWnd = wnd;
                conn->RtoMs = TcpInitialRtoMs;
                if (conn->SndUna == conn->SndNxt)
                    conn->RetransmitDeadlineMs = 0; /* all acked */
                else
                    conn->RetransmitDeadlineMs = now + conn->RtoMs;
            }
        }

        /* Data processing -- in-order only */
        if (payloadLen > 0 && seq == conn->RcvNxt)
        {
            ulong written = conn->RecvBuf.Write(payload, payloadLen);
            conn->RcvNxt += (u32)written;
            conn->RcvWnd = (u32)conn->RecvBuf.Free();
            conn->DataReady.Set(1);

            /* Send ACK */
            u32 savedNxt = conn->SndNxt;
            SendSegment(conn, TcpFlagAck, nullptr, 0);
            conn->SndNxt = savedNxt;
        }
        else if (payloadLen > 0 && seq != conn->RcvNxt)
        {
            /* Out-of-order: send duplicate ACK */
            u32 savedNxt = conn->SndNxt;
            SendSegment(conn, TcpFlagAck, nullptr, 0);
            conn->SndNxt = savedNxt;
        }

        /* FIN processing (passive close) -- only accept in-order FIN */
        if ((flags & TcpFlagFin) && (seq + (u32)payloadLen == conn->RcvNxt))
        {
            conn->RcvNxt = seq + (u32)payloadLen + 1;
            conn->State = TcpStateCloseWait;
            conn->DataReady.Set(1); /* wake Recv so it returns 0 */

            u32 savedNxt = conn->SndNxt;
            SendSegment(conn, TcpFlagAck, nullptr, 0);
            conn->SndNxt = savedNxt;
        }
        break;
    }
    case TcpStateFinWait1:
    {
        if (flags & TcpFlagAck)
        {
            if (ack == conn->SndNxt)
            {
                conn->SndUna = ack;
                conn->FinAcked = true;
            }
        }

        if (flags & TcpFlagFin)
        {
            conn->RcvNxt = seq + 1;

            u32 savedNxt = conn->SndNxt;
            SendSegment(conn, TcpFlagAck, nullptr, 0);
            conn->SndNxt = savedNxt;

            if (conn->FinAcked)
            {
                /* Our FIN was ACKed and we received their FIN */
                conn->State = TcpStateTimeWait;
                conn->TimeWaitDeadlineMs = GetBootTimeMs() + TcpTimeWaitMs;
            }
            else
            {
                /* Simultaneous close -- they FINed but haven't ACKed ours */
                conn->State = TcpStateClosing;
            }
        }
        else if (conn->FinAcked)
        {
            /* Our FIN was ACKed, waiting for their FIN */
            conn->State = TcpStateFinWait2;
            conn->RetransmitDeadlineMs = 0;
        }
        break;
    }
    case TcpStateFinWait2:
    {
        if (flags & TcpFlagFin)
        {
            conn->RcvNxt = seq + 1;

            u32 savedNxt = conn->SndNxt;
            SendSegment(conn, TcpFlagAck, nullptr, 0);
            conn->SndNxt = savedNxt;

            conn->State = TcpStateTimeWait;
            conn->TimeWaitDeadlineMs = GetBootTimeMs() + TcpTimeWaitMs;
        }
        break;
    }
    case TcpStateClosing:
    {
        if ((flags & TcpFlagAck) && ack == conn->SndNxt)
        {
            conn->State = TcpStateTimeWait;
            conn->TimeWaitDeadlineMs = GetBootTimeMs() + TcpTimeWaitMs;
        }
        break;
    }
    case TcpStateLastAck:
    {
        if ((flags & TcpFlagAck) && ack == conn->SndNxt)
        {
            conn->State = TcpStateClosed;
            conn->ConnReady.Set(1);
        }
        break;
    }
    case TcpStateCloseWait:
    {
        /* ACK processing for any pending data */
        if (flags & TcpFlagAck)
        {
            long ackAdvance = (long)(ack - conn->SndUna);
            if (ackAdvance > 0 && ack <= conn->SndNxt)
            {
                conn->SendBuf.Consume((ulong)ackAdvance);
                conn->SndUna = ack;
            }
        }
        break;
    }
    case TcpStateTimeWait:
    {
        /* Retransmitted FIN -- re-ACK */
        if (flags & TcpFlagFin)
        {
            u32 savedNxt = conn->SndNxt;
            SendSegment(conn, TcpFlagAck, nullptr, 0);
            conn->SndNxt = savedNxt;
            conn->TimeWaitDeadlineMs = GetBootTimeMs() + TcpTimeWaitMs;
        }
        break;
    }
    default:
        break;
    }
}

/* --- Process incoming TCP segment --- */

void Tcp::Process(NetDevice* dev, const u8* frame, ulong frameLen)
{
    if (frameLen < sizeof(EthHdr) + sizeof(IpHdr) + sizeof(TcpHdr))
    {
        RxTooShort.Inc();
        return;
    }

    const EthHdr* eth = (const EthHdr*)frame;
    const IpHdr* ip = (const IpHdr*)(frame + sizeof(EthHdr));
    ulong ipHdrLen = (ip->VersionIhl & 0x0F) * 4;
    if (ipHdrLen < sizeof(IpHdr))
    {
        RxTooShort.Inc();
        return;
    }

    ulong ipTotalLen = Ntohs(ip->TotalLen);
    if (sizeof(EthHdr) + ipTotalLen > frameLen)
    {
        RxTooShort.Inc();
        return;
    }

    const TcpHdr* tcp = (const TcpHdr*)(frame + sizeof(EthHdr) + ipHdrLen);
    ulong tcpLen = ipTotalLen - ipHdrLen;
    if (tcpLen < sizeof(TcpHdr))
    {
        RxTooShort.Inc();
        return;
    }

    /* Verify TCP checksum */
    u16 ck = TcpChecksum(ip->SrcAddr, ip->DstAddr, tcp, tcpLen);
    if (ck != 0)
    {
        RxChecksumErr.Inc();
        return;
    }

    RxSegments.Inc();

    ulong tcpHdrLen = ((tcp->DataOff >> 4) & 0xF) * 4;
    if (tcpHdrLen < sizeof(TcpHdr) || tcpHdrLen > tcpLen)
    {
        RxTooShort.Inc();
        return;
    }

    const u8* payload = (const u8*)tcp + tcpHdrLen;
    ulong payloadLen = tcpLen - tcpHdrLen;

    u32 localIp = Ntohl(ip->DstAddr);
    u16 localPort = Ntohs(tcp->DstPort);
    u32 remoteIp = Ntohl(ip->SrcAddr);
    u16 remotePort = Ntohs(tcp->SrcPort);

    PoolLock.Lock();

    /* Try exact 4-tuple match first */
    TcpConn* conn = LookupLocked(localIp, localPort, remoteIp, remotePort);
    if (conn)
    {
        PoolLock.Unlock();
        HandleState(conn, ip, tcp, payload, payloadLen);
        conn->Lock.Unlock();
        return;
    }

    /* Try listener match (incoming SYN) */
    if (tcp->Flags & TcpFlagSyn)
    {
        TcpConn* listener = FindListenerLocked(localPort);
        if (listener)
        {
            /* Allocate a new connection for this SYN */
            TcpConn* newConn = AllocConn();
            if (!newConn)
            {
                listener->Lock.Unlock();
                PoolLock.Unlock();
                return;
            }

            /* Setup new connection */
            newConn->Dev = dev;
            newConn->LocalIp = IpAddress(localIp);
            newConn->LocalPort = localPort;
            newConn->RemoteIp = IpAddress(remoteIp);
            newConn->RemotePort = remotePort;
            newConn->State = TcpStateSynReceived;
            newConn->Irs = Ntohl(tcp->SeqNum);
            newConn->RcvNxt = newConn->Irs + 1;
            newConn->Iss = (u32)(ReadTsc() & 0xFFFFFFFF);
            newConn->SndNxt = newConn->Iss;
            newConn->SndUna = newConn->Iss;
            newConn->SndWnd = Ntohs(tcp->Window);
            newConn->PeerMss = ParseMssOption(tcp);

            /* Resolve MAC from the incoming frame's source */
            newConn->ResolvedMac = MacAddress(eth->SrcMac);

            InsertHash(newConn);
            newConn->Lock.Lock();
            listener->Lock.Unlock();
            PoolLock.Unlock();

            /* Send SYN-ACK */
            SendSegment(newConn, TcpFlagSyn | TcpFlagAck, nullptr, 0);
            newConn->SndNxt++; /* SYN consumes one sequence number */
            newConn->RetransmitDeadlineMs = GetBootTimeMs() + newConn->RtoMs;

            ConnCount.Inc();
            newConn->Lock.Unlock();
            return;
        }
    }

    PoolLock.Unlock();

    /* No connection found -- send RST for non-RST segments */
    if (!(tcp->Flags & TcpFlagRst))
    {
        MacAddress srcMac(eth->SrcMac);
        u32 rstSeq = 0;
        u32 rstAck = Ntohl(tcp->SeqNum) + payloadLen;
        if (tcp->Flags & TcpFlagSyn)
            rstAck++;
        if (tcp->Flags & TcpFlagAck)
            rstSeq = Ntohl(tcp->AckNum);

        SendRst(dev, srcMac,
                IpAddress(localIp), IpAddress(remoteIp),
                localPort, remotePort, rstSeq, rstAck);
    }
}

/* --- User-facing API --- */

TcpConn* Tcp::Connect(NetDevice* dev, IpAddress dstIp, u16 dstPort, u16 srcPort)
{
    if (!Initialized)
        return nullptr;

    /* Step 1: ARP resolve (blocking, no locks).
       For off-subnet destinations, resolve the gateway MAC. */
    IpAddress arpTarget = dev->RouteIp(dstIp);
    MacAddress dstMac;
    if (!ArpTable::GetInstance().Resolve(dev, arpTarget, dstMac))
    {
        Trace(0, "Tcp: ARP failed for Connect");
        return nullptr;
    }

    /* Step 2-8: Allocate under locks */
    PortMutex.Lock();
    PoolLock.Lock();

    if (srcPort == 0)
    {
        srcPort = AllocEphemeralPort();
        if (srcPort == 0)
        {
            PoolLock.Unlock();
            PortMutex.Unlock();
            Trace(0, "Tcp: no ephemeral ports available");
            return nullptr;
        }
    }

    TcpConn* conn = AllocConn();
    if (!conn)
    {
        PoolLock.Unlock();
        PortMutex.Unlock();
        Trace(0, "Tcp: no free connections");
        return nullptr;
    }

    conn->Dev = dev;
    conn->LocalIp = dev->GetIp();
    conn->LocalPort = srcPort;
    conn->RemoteIp = dstIp;
    conn->RemotePort = dstPort;
    conn->ResolvedMac = dstMac;
    conn->Iss = (u32)(ReadTsc() & 0xFFFFFFFF);
    conn->SndNxt = conn->Iss;
    conn->SndUna = conn->Iss;
    conn->State = TcpStateSynSent;

    InsertHash(conn);
    conn->Lock.Lock();
    PoolLock.Unlock();
    PortMutex.Unlock();

    /* Send SYN */
    SendSegment(conn, TcpFlagSyn, nullptr, 0);
    conn->SndNxt++; /* SYN consumes one sequence number */
    conn->RetransmitDeadlineMs = GetBootTimeMs() + conn->RtoMs;
    conn->Lock.Unlock();

    ConnCount.Inc();

    /* Sleep-poll for connection established */
    ulong deadline = GetBootTimeMs() + TcpConnectTimeoutMs;
    while (GetBootTimeMs() < deadline)
    {
        if (conn->ConnReady.Get())
        {
            conn->Lock.Lock();
            TcpState st = conn->State;
            conn->Lock.Unlock();
            if (st == TcpStateEstablished)
                return conn;
            /* Connection failed (RST, etc.) */
            Close(conn);
            return nullptr;
        }
        Sleep(1 * Const::NanoSecsInMs);
    }

    /* Timeout */
    Trace(0, "Tcp: connect timeout %u -> %u",
          (ulong)srcPort, (ulong)dstPort);
    Close(conn);
    return nullptr;
}

TcpConn* Tcp::Listen(NetDevice* dev, u16 port)
{
    if (!Initialized)
        return nullptr;

    PoolLock.Lock();
    TcpConn* conn = AllocConn();
    if (!conn)
    {
        PoolLock.Unlock();
        return nullptr;
    }

    conn->Dev = dev;
    conn->LocalIp = dev->GetIp();
    conn->LocalPort = port;
    conn->State = TcpStateListen;
    /* Listener is NOT in the hash table -- FindListenerLocked scans the pool */
    PoolLock.Unlock();
    return conn;
}

TcpConn* Tcp::Accept(TcpConn* listener)
{
    if (!listener || listener->State != TcpStateListen)
        return nullptr;

    for (;;)
    {
        PoolLock.Lock();
        for (ulong i = 0; i < TcpMaxConnections; i++)
        {
            TcpConn* c = &Pool[i];
            if (c == listener)
                continue;
            if (c->LocalPort == listener->LocalPort &&
                (c->State == TcpStateEstablished ||
                 c->State == TcpStateSynReceived))
            {
                c->Lock.Lock();
                if (c->State == TcpStateEstablished)
                {
                    PoolLock.Unlock();
                    c->Lock.Unlock();
                    return c;
                }
                c->Lock.Unlock();
            }
        }
        PoolLock.Unlock();
        Sleep(1 * Const::NanoSecsInMs);
    }
}

long Tcp::Send(TcpConn* conn, const void* data, ulong len)
{
    if (!conn)
        return -1;

    const u8* src = (const u8*)data;
    ulong sent = 0;

    while (sent < len)
    {
        conn->Lock.Lock();

        if (conn->State != TcpStateEstablished &&
            conn->State != TcpStateCloseWait)
        {
            conn->Lock.Unlock();
            return (sent > 0) ? (long)sent : -1;
        }

        ulong avail = conn->SendBuf.Free();
        if (avail == 0)
        {
            conn->Lock.Unlock();
            Sleep(1 * Const::NanoSecsInMs);
            continue;
        }

        ulong chunk = len - sent;
        if (chunk > avail)
            chunk = avail;

        /* Limit to PeerMss for segment sizing */
        if (chunk > conn->PeerMss)
            chunk = conn->PeerMss;

        conn->SendBuf.Write(src + sent, chunk);

        /* Send data segment -- peek the data we just wrote */
        u8 segData[TcpOurMss];
        ulong peekOff = conn->SendBuf.Used() - chunk;
        ulong segLen = conn->SendBuf.Peek(segData, peekOff, chunk);

        SendSegment(conn, TcpFlagAck | TcpFlagPsh, segData, segLen);
        conn->SndNxt += (u32)segLen;

        if (conn->RetransmitDeadlineMs == 0)
            conn->RetransmitDeadlineMs = GetBootTimeMs() + conn->RtoMs;

        conn->Lock.Unlock();
        sent += chunk;
    }

    return (long)sent;
}

long Tcp::Recv(TcpConn* conn, void* buf, ulong len)
{
    if (!conn)
        return -1;

    u8* dst = (u8*)buf;

    for (;;)
    {
        conn->Lock.Lock();

        ulong avail = conn->RecvBuf.Used();
        if (avail > 0)
        {
            ulong got = conn->RecvBuf.Read(dst, len);
            conn->RcvWnd = (u32)conn->RecvBuf.Free();
            if (conn->RecvBuf.Used() == 0)
                conn->DataReady.Set(0);
            conn->Lock.Unlock();
            return (long)got;
        }

        /* No data: check if connection is closing */
        if (conn->State == TcpStateCloseWait ||
            conn->State == TcpStateClosed ||
            conn->State == TcpStateTimeWait ||
            conn->State == TcpStateLastAck ||
            conn->State == TcpStateClosing)
        {
            conn->Lock.Unlock();
            return 0; /* EOF */
        }

        conn->Lock.Unlock();
        Sleep(1 * Const::NanoSecsInMs);
    }
}

void Tcp::Close(TcpConn* conn)
{
    if (!conn)
        return;

    conn->Lock.Lock();

    switch (conn->State)
    {
    case TcpStateEstablished:
    case TcpStateSynReceived:
    {
        conn->State = TcpStateFinWait1;
        conn->FinAcked = false;
        SendSegment(conn, TcpFlagFin | TcpFlagAck, nullptr, 0);
        conn->SndNxt++;
        conn->RetransmitDeadlineMs = GetBootTimeMs() + conn->RtoMs;
        break;
    }
    case TcpStateCloseWait:
    {
        conn->State = TcpStateLastAck;
        SendSegment(conn, TcpFlagFin | TcpFlagAck, nullptr, 0);
        conn->SndNxt++;
        conn->RetransmitDeadlineMs = GetBootTimeMs() + conn->RtoMs;
        break;
    }
    case TcpStateSynSent:
    {
        conn->State = TcpStateClosed;
        break;
    }
    case TcpStateListen:
    {
        conn->State = TcpStateFree;
        break;
    }
    default:
        break;
    }

    conn->Lock.Unlock();
}

/* --- Retransmit / cleanup timer --- */

void Tcp::ProcessRetransmits()
{
    ulong now = GetBootTimeMs();
    bool anyCleanup = false;

    /* Phase 1: retransmit + TimeWait expiry (no PoolLock) */
    for (ulong i = 0; i < TcpMaxConnections; i++)
    {
        TcpConn* conn = &Pool[i];
        conn->Lock.Lock();

        if (conn->State == TcpStateFree)
        {
            conn->Lock.Unlock();
            continue;
        }

        /* TimeWait -> Closed */
        if (conn->State == TcpStateTimeWait &&
            conn->TimeWaitDeadlineMs != 0 &&
            now >= conn->TimeWaitDeadlineMs)
        {
            conn->State = TcpStateClosed;
            conn->NeedCleanup = true;
            anyCleanup = true;
            conn->Lock.Unlock();
            continue;
        }

        /* Closed -> mark for cleanup */
        if (conn->State == TcpStateClosed)
        {
            conn->NeedCleanup = true;
            anyCleanup = true;
            conn->Lock.Unlock();
            continue;
        }

        /* Retransmit check */
        if (conn->RetransmitDeadlineMs != 0 &&
            now >= conn->RetransmitDeadlineMs)
        {
            bool retransmitted = false;

            /* SYN / SYN-ACK retransmit -- state-based, not data-based */
            if (conn->State == TcpStateSynSent)
            {
                conn->SndNxt = conn->SndUna;
                SendSegment(conn, TcpFlagSyn, nullptr, 0);
                conn->SndNxt++;
                retransmitted = true;
            }
            else if (conn->State == TcpStateSynReceived)
            {
                conn->SndNxt = conn->SndUna;
                SendSegment(conn, TcpFlagSyn | TcpFlagAck, nullptr, 0);
                conn->SndNxt++;
                retransmitted = true;
            }
            /* FIN retransmit */
            else if (conn->State == TcpStateFinWait1 ||
                     conn->State == TcpStateLastAck ||
                     conn->State == TcpStateClosing)
            {
                /* Retransmit unacked data first if any */
                ulong bufUsed = conn->SendBuf.Used();
                if (bufUsed > 0)
                {
                    ulong segLen = bufUsed;
                    if (segLen > conn->PeerMss)
                        segLen = conn->PeerMss;

                    u8 segData[TcpOurMss];
                    ulong got = conn->SendBuf.Peek(segData, 0, segLen);
                    if (got > 0)
                    {
                        u32 savedNxt = conn->SndNxt;
                        conn->SndNxt = conn->SndUna;
                        SendSegment(conn, TcpFlagAck | TcpFlagPsh, segData, got);
                        conn->SndNxt = savedNxt;
                    }
                }
                else
                {
                    conn->SndNxt = conn->SndUna;
                    SendSegment(conn, TcpFlagFin | TcpFlagAck, nullptr, 0);
                    conn->SndNxt++;
                }
                retransmitted = true;
            }
            /* Data retransmit for Established / CloseWait */
            else if (conn->SndUna != conn->SndNxt &&
                     conn->SendBuf.Used() > 0)
            {
                ulong segLen = conn->SendBuf.Used();
                if (segLen > conn->PeerMss)
                    segLen = conn->PeerMss;

                u8 segData[TcpOurMss];
                ulong got = conn->SendBuf.Peek(segData, 0, segLen);
                if (got > 0)
                {
                    u32 savedNxt = conn->SndNxt;
                    conn->SndNxt = conn->SndUna;
                    SendSegment(conn, TcpFlagAck | TcpFlagPsh, segData, got);
                    conn->SndNxt = savedNxt;
                }
                retransmitted = true;
            }

            if (retransmitted)
            {
                Retransmits.Inc();
                conn->RtoMs *= 2;
                if (conn->RtoMs > TcpMaxRtoMs)
                    conn->RtoMs = TcpMaxRtoMs;
            }
            conn->RetransmitDeadlineMs = now + conn->RtoMs;
        }

        conn->Lock.Unlock();
    }

    /* Phase 2: cleanup (acquire PoolLock) */
    if (anyCleanup)
    {
        PoolLock.Lock();
        for (ulong i = 0; i < TcpMaxConnections; i++)
        {
            TcpConn* conn = &Pool[i];
            conn->Lock.Lock();
            if (conn->NeedCleanup && conn->State == TcpStateClosed)
            {
                RemoveHash(conn);
                conn->State = TcpStateFree;
                conn->NeedCleanup = false;
                ConnCount.Dec();
            }
            else
            {
                conn->NeedCleanup = false;
            }
            conn->Lock.Unlock();
        }
        PoolLock.Unlock();
    }
}

/* --- Dump / stats --- */

const char* Tcp::StateToString(TcpState state)
{
    switch (state)
    {
    case TcpStateFree:          return "FREE";
    case TcpStateListen:        return "LISTEN";
    case TcpStateSynSent:       return "SYN_SENT";
    case TcpStateSynReceived:   return "SYN_RCVD";
    case TcpStateEstablished:   return "ESTABLISHED";
    case TcpStateFinWait1:      return "FIN_WAIT_1";
    case TcpStateFinWait2:      return "FIN_WAIT_2";
    case TcpStateCloseWait:     return "CLOSE_WAIT";
    case TcpStateLastAck:       return "LAST_ACK";
    case TcpStateClosing:       return "CLOSING";
    case TcpStateTimeWait:      return "TIME_WAIT";
    case TcpStateClosed:        return "CLOSED";
    default:                    return "???";
    }
}

void Tcp::Dump(Stdlib::Printer& printer)
{
    printer.Printf("TCP stats: tx=%u rx=%u rxerr=%u retx=%u conns=%u\n",
                   (ulong)TxSegments.Get(), (ulong)RxSegments.Get(),
                   (ulong)RxChecksumErr.Get(), (ulong)Retransmits.Get(),
                   (ulong)ConnCount.Get());

    for (ulong i = 0; i < TcpMaxConnections; i++)
    {
        TcpConn* conn = &Pool[i];
        conn->Lock.Lock();
        if (conn->State != TcpStateFree)
        {
            printer.Printf("  [%u] ", i);
            conn->LocalIp.Print(printer);
            printer.Printf(":%u -> ", (ulong)conn->LocalPort);
            conn->RemoteIp.Print(printer);
            printer.Printf(":%u  %s  snd=%u/%u rcv=%u\n",
                           (ulong)conn->RemotePort,
                           StateToString(conn->State),
                           (ulong)conn->SendBuf.Used(),
                           (ulong)(conn->SndNxt - conn->SndUna),
                           (ulong)conn->RecvBuf.Used());
        }
        conn->Lock.Unlock();
    }
}

} /* namespace Kernel */
