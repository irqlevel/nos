#pragma once

#include <include/types.h>
#include <net/net.h>
#include <lib/printer.h>

namespace Kernel
{

struct NetDevice;

/* TCP itself is Rust (src/rust/net/src/tcp.rs): the connection pool, the
   state machine, the retransmit and persist timers, and the blocking calls
   over them. What is left here is the way in.

   A connection is opaque on this side: the pool lives in Rust and a
   TcpConn* is a pointer into it, stable for the connection's life. */
struct TcpConn;

/* What Tcp::Recv answers below zero */
static const long TcpRecvError   = -1;
static const long TcpRecvTimeout = -2;

class Tcp
{
public:
    static Tcp& GetInstance()
    {
        static Tcp instance;
        return instance;
    }

    bool Init();

    /* Active open -- blocks until connected or the timeout passes.
       srcPort = 0 takes an ephemeral port. */
    TcpConn* Connect(NetDevice* dev, Net::IpAddress dstIp, u16 dstPort, u16 srcPort = 0);

    /* Passive open, at every address the machine has */
    TcpConn* Listen(NetDevice* dev, u16 port);

    /* The next connection on a listening socket; nullptr once timeoutMs
       (0 = wait forever) passes with none, or once the listener is closed. */
    TcpConn* Accept(TcpConn* listener, ulong timeoutMs = 0);

    /* Send returns the bytes queued, or -1 when the connection is gone
       before any were. Recv returns the byte count, 0 at end of stream,
       TcpRecvError on a bad argument, or TcpRecvTimeout when timeoutMs
       elapses with nothing received. */
    long Send(TcpConn* conn, const void* data, ulong len, ulong timeoutMs = 0);
    long Recv(TcpConn* conn, void* buf, ulong len, ulong timeoutMs = 0);

    /* The graceful close; closing a listener resets the connections that
       arrived on its port and were never accepted. */
    void Close(TcpConn* conn);

    /* A reset instead of the FIN exchange, and the slot back at the next
       tick rather than after a minute of TIME-WAIT. */
    void Abort(TcpConn* conn);

    /* Who the connection is with; host byte order. */
    void Peer(TcpConn* conn, u32& ip, u16& port);

    /* Called from a net device's receive dispatch for IpProtoTcp */
    void Process(NetDevice* dev, const u8* frame, ulong frameLen);

    /* Called by Icmp for a hard Destination Unreachable quoting a segment
       we sent. */
    void OnIcmpUnreachable(u32 localIp, u16 localPort,
                           u32 remoteIp, u16 remotePort, u32 quotedSeq);

    void Dump(Stdlib::Printer& printer);

private:
    Tcp() {}
    ~Tcp() {}
    Tcp(const Tcp& other) = delete;
    Tcp(Tcp&& other) = delete;
    Tcp& operator=(const Tcp& other) = delete;
    Tcp& operator=(Tcp&& other) = delete;
};

} /* namespace Kernel */
