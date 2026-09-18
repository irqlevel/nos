#include "tcp.h"

#include <net/net_device.h>

extern "C" {

/* What `tcpstat` reports; crate::tcp::Stats is the same struct. */
struct RustTcpStats
{
    unsigned long Tx;
    unsigned long Rx;
    unsigned long RxErr;
    unsigned long RxShort;
    unsigned long Retransmits;
    unsigned long Conns;
};

/* One connection's line; crate::abi::TcpConnLine is the same struct. */
struct RustTcpConnLine
{
    const char* State;
    unsigned int LocalIp;
    unsigned short LocalPort;
    unsigned short RemotePort;
    unsigned int RemoteIp;
    unsigned long SendUsed;
    unsigned long InFlight;
    unsigned long RecvUsed;
};

int rust_tcp_init();
void rust_tcp_process(unsigned long dev, const unsigned char* data, unsigned long len);
void* rust_tcp_connect(unsigned long dev, unsigned int dstIp, unsigned short dstPort,
    unsigned short srcPort);
void* rust_tcp_listen(unsigned long dev, unsigned short port);
void* rust_tcp_accept(void* listener, unsigned long timeoutMs);
long rust_tcp_send(void* conn, const unsigned char* data, unsigned long len,
    unsigned long timeoutMs);
long rust_tcp_recv(void* conn, unsigned char* buf, unsigned long len,
    unsigned long timeoutMs);
void rust_tcp_close(void* conn);
void rust_tcp_abort(void* conn);
void rust_tcp_peer(void* conn, unsigned int* ip, unsigned short* port);
void rust_tcp_on_icmp_unreachable(unsigned int localIp, unsigned short localPort,
    unsigned int remoteIp, unsigned short remotePort, unsigned int quotedSeq);
void rust_tcp_stats(RustTcpStats* out);
int rust_tcp_conn_at(unsigned long index, RustTcpConnLine* out);
unsigned long rust_tcp_max_connections();

}

namespace Kernel
{

bool Tcp::Init()
{
    return rust_tcp_init() == 0;
}

TcpConn* Tcp::Connect(NetDevice* dev, Net::IpAddress dstIp, u16 dstPort, u16 srcPort)
{
    if (dev == nullptr)
        return nullptr;

    return (TcpConn*)rust_tcp_connect(reinterpret_cast<unsigned long>(dev),
        dstIp.Addr4, dstPort, srcPort);
}

TcpConn* Tcp::Listen(NetDevice* dev, u16 port)
{
    if (dev == nullptr)
        return nullptr;

    return (TcpConn*)rust_tcp_listen(reinterpret_cast<unsigned long>(dev), port);
}

TcpConn* Tcp::Accept(TcpConn* listener, ulong timeoutMs)
{
    return (TcpConn*)rust_tcp_accept(listener, timeoutMs);
}

long Tcp::Send(TcpConn* conn, const void* data, ulong len, ulong timeoutMs)
{
    return rust_tcp_send(conn, (const unsigned char*)data, len, timeoutMs);
}

long Tcp::Recv(TcpConn* conn, void* buf, ulong len, ulong timeoutMs)
{
    return rust_tcp_recv(conn, (unsigned char*)buf, len, timeoutMs);
}

void Tcp::Close(TcpConn* conn)
{
    rust_tcp_close(conn);
}

void Tcp::Abort(TcpConn* conn)
{
    rust_tcp_abort(conn);
}

void Tcp::Peer(TcpConn* conn, u32& ip, u16& port)
{
    ip = 0;
    port = 0;
    rust_tcp_peer(conn, &ip, &port);
}

void Tcp::Process(NetDevice* dev, const u8* frame, ulong frameLen)
{
    if (dev == nullptr || frame == nullptr)
        return;

    rust_tcp_process(reinterpret_cast<unsigned long>(dev), frame, frameLen);
}

void Tcp::OnIcmpUnreachable(u32 localIp, u16 localPort,
                            u32 remoteIp, u16 remotePort, u32 quotedSeq)
{
    rust_tcp_on_icmp_unreachable(localIp, localPort, remoteIp, remotePort, quotedSeq);
}

void Tcp::Dump(Stdlib::Printer& printer)
{
    RustTcpStats stats = {};
    rust_tcp_stats(&stats);

    printer.Printf("TCP stats: tx=%u rx=%u rxerr=%u retx=%u conns=%u\n",
        stats.Tx, stats.Rx, stats.RxErr, stats.Retransmits, stats.Conns);

    ulong count = rust_tcp_max_connections();
    for (ulong i = 0; i < count; i++)
    {
        RustTcpConnLine line = {};
        if (rust_tcp_conn_at(i, &line) != 0)
            continue;

        Net::IpAddress localIp;
        localIp.Addr4 = line.LocalIp;
        Net::IpAddress remoteIp;
        remoteIp.Addr4 = line.RemoteIp;

        printer.Printf("  [%u] ", i);
        localIp.Print(printer);
        printer.Printf(":%u -> ", (ulong)line.LocalPort);
        remoteIp.Print(printer);
        printer.Printf(":%u  %s  snd=%u/%u rcv=%u\n",
            (ulong)line.RemotePort, line.State,
            line.SendUsed, line.InFlight, line.RecvUsed);
    }
}

} /* namespace Kernel */
