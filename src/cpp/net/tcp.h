#pragma once

#include <include/types.h>
#include <include/const.h>
#include <net/net.h>
#include <net/net_device.h>
#include <kernel/raw_spin_lock.h>
#include <kernel/mutex.h>
#include <kernel/atomic.h>
#include <kernel/timer.h>
#include <lib/list_entry.h>
#include <lib/printer.h>

namespace Kernel
{

/* TCP flag constants */
static const u8 TcpFlagFin = 0x01;
static const u8 TcpFlagSyn = 0x02;
static const u8 TcpFlagRst = 0x04;
static const u8 TcpFlagPsh = 0x08;
static const u8 TcpFlagAck = 0x10;

/* TCP option constants */
static const u8 TcpOptEnd      = 0;
static const u8 TcpOptNop      = 1;
static const u8 TcpOptMss      = 2;
static const u8 TcpOptMssLen   = 4;

/* TCP constants */
static const ulong TcpMaxConnections   = 64;
static const ulong TcpSendBufSize      = 8192;
static const ulong TcpRecvBufSize      = 8192;
static const u16   TcpDefaultMss       = 536;
static const u16   TcpOurMss           = 1460;
static const ulong TcpInitialRtoMs     = 1000;
static const ulong TcpMaxRtoMs         = 8000;
static const ulong TcpTimeWaitMs       = 2000;
static const ulong TcpConnectTimeoutMs = 5000;
static const u8    TcpDefaultTtl       = 64;
static const ulong TcpTimerPeriodMs    = 200;
static const ulong TcpConnHashSize     = 32;
static const u16   TcpEphemeralPortBase = 49152;
static const u16   TcpEphemeralPortMax  = 65535;

/* DataOff value for a 20-byte header (5 * 4 = 20) */
static const u8 TcpDataOff5 = (5 << 4);
/* DataOff value for a 24-byte header (6 * 4 = 24, with MSS option) */
static const u8 TcpDataOff6 = (6 << 4);

/* TCP connection states */
enum TcpState : u8
{
    TcpStateFree = 0,
    TcpStateListen,
    TcpStateSynSent,
    TcpStateSynReceived,
    TcpStateEstablished,
    TcpStateFinWait1,
    TcpStateFinWait2,
    TcpStateCloseWait,
    TcpStateLastAck,
    TcpStateClosing,
    TcpStateTimeWait,
    TcpStateClosed,
};

/* Simple byte ring buffer */
struct TcpRingBuf
{
    u8 Data[TcpSendBufSize]; /* reuse max of Send/Recv size */
    ulong Head;
    ulong Tail;
    ulong Size; /* capacity */

    void Init(ulong capacity)
    {
        Head = 0;
        Tail = 0;
        Size = capacity;
    }

    ulong Used() const
    {
        return (Tail - Head);
    }

    ulong Free() const
    {
        return Size - Used();
    }

    ulong Write(const u8* src, ulong len)
    {
        ulong avail = Free();
        if (len > avail)
            len = avail;
        for (ulong i = 0; i < len; i++)
            Data[(Tail + i) % Size] = src[i];
        Tail += len;
        return len;
    }

    ulong Read(u8* dst, ulong len)
    {
        ulong avail = Used();
        if (len > avail)
            len = avail;
        for (ulong i = 0; i < len; i++)
            dst[i] = Data[(Head + i) % Size];
        Head += len;
        return len;
    }

    /* Peek at data without advancing Head */
    ulong Peek(u8* dst, ulong offset, ulong len) const
    {
        ulong avail = Used();
        if (offset >= avail)
            return 0;
        if (len > avail - offset)
            len = avail - offset;
        for (ulong i = 0; i < len; i++)
            dst[i] = Data[(Head + offset + i) % Size];
        return len;
    }

    /* Discard len bytes from the front */
    void Consume(ulong len)
    {
        ulong avail = Used();
        if (len > avail)
            len = avail;
        Head += len;
    }
};

/* Per-connection state */
struct TcpConn
{
    /* Identity */
    Net::IpAddress LocalIp;
    u16 LocalPort;
    Net::IpAddress RemoteIp;
    u16 RemotePort;
    NetDevice* Dev;
    Net::MacAddress ResolvedMac;

    /* State */
    TcpState State;

    /* Sequence tracking */
    u32 SndUna;   /* oldest unacked */
    u32 SndNxt;   /* next to send */
    u32 SndWnd;   /* peer window */
    u32 RcvNxt;   /* next expected */
    u32 RcvWnd;   /* our window */
    u32 Iss;      /* initial send sequence */
    u32 Irs;      /* initial receive sequence */
    u16 PeerMss;

    /* Buffers */
    TcpRingBuf SendBuf;
    TcpRingBuf RecvBuf;

    /* Retransmit state */
    ulong RtoMs;
    ulong RetransmitDeadlineMs; /* boot-time ms when retransmit fires */
    ulong TimeWaitDeadlineMs;

    /* Flags */
    Atomic DataReady;   /* set when data arrives in RecvBuf */
    Atomic ConnReady;   /* set when state changes from SynSent/SynReceived */
    bool NeedCleanup;
    bool FinAcked;      /* our FIN has been ACKed */

    /* Per-connection lock */
    RawSpinLock Lock;

    /* Hash table linkage */
    Stdlib::ListEntry HashLink;

    void Init();
    void Reset();
};

/* TCP singleton */
class Tcp : public TimerCallback
{
public:
    static Tcp& GetInstance()
    {
        static Tcp instance;
        return instance;
    }

    bool Init();

    /* Active open -- blocks until connected or timeout.
       srcPort=0 means auto-allocate an ephemeral port. */
    TcpConn* Connect(NetDevice* dev, Net::IpAddress dstIp, u16 dstPort, u16 srcPort = 0);

    /* Passive open -- marks a port as listening */
    TcpConn* Listen(NetDevice* dev, u16 port);

    /* Accept -- blocks until a new connection arrives on a listening socket */
    TcpConn* Accept(TcpConn* listener);

    /* Data transfer -- blocks until data sent/received or timeout */
    long Send(TcpConn* conn, const void* data, ulong len);
    long Recv(TcpConn* conn, void* buf, ulong len);

    /* Close connection (graceful FIN exchange) */
    void Close(TcpConn* conn);

    /* Called from VirtioNet::ProcessRx for IpProtoTcp */
    void Process(NetDevice* dev, const u8* frame, ulong frameLen);

    /* Called from TypeTcpTimer SoftIrq handler -- retransmits + cleanup */
    void ProcessRetransmits();

    /* TimerCallback -- runs in IPI context, just raises SoftIrq */
    void OnTick(TimerCallback& callback) override;

    void Dump(Stdlib::Printer& printer);

private:
    Tcp();
    ~Tcp();
    Tcp(const Tcp& other) = delete;
    Tcp(Tcp&& other) = delete;
    Tcp& operator=(const Tcp& other) = delete;
    Tcp& operator=(Tcp&& other) = delete;

    Mutex PortMutex;
    RawSpinLock PoolLock;
    TcpConn Pool[TcpMaxConnections];
    Stdlib::ListEntry HashTable[TcpConnHashSize];
    u16 NextEphemeralPort;
    bool Initialized;

    /* Statistics */
    Atomic TxSegments;
    Atomic RxSegments;
    Atomic RxChecksumErr;
    Atomic RxTooShort;
    Atomic Retransmits;
    Atomic ConnCount;

    u16 AllocEphemeralPort();
    TcpConn* AllocConn();
    TcpConn* LookupLocked(u32 localIp, u16 localPort,
                          u32 remoteIp, u16 remotePort);
    TcpConn* FindListenerLocked(u16 localPort);
    void InsertHash(TcpConn* conn);
    void RemoveHash(TcpConn* conn);
    ulong HashIndex(u32 localIp, u16 localPort,
                    u32 remoteIp, u16 remotePort);

    void SendSegment(TcpConn* conn, u8 flags, const u8* data, ulong len);
    void SendRst(NetDevice* dev, const Net::MacAddress& dstMac,
                 Net::IpAddress srcIp, Net::IpAddress dstIp,
                 u16 srcPort, u16 dstPort, u32 seq, u32 ack);

    void HandleState(TcpConn* conn, const Net::IpHdr* ip,
                     const Net::TcpHdr* tcp, const u8* payload,
                     ulong payloadLen);

    ulong GetBootTimeMs();

    static void TcpTimerSoftIrqHandler(void* ctx);

    static const char* StateToString(TcpState state);
};

} /* namespace Kernel */
