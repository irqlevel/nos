#include "tls.h"
#include "tcp.h"
#include <kernel/trace.h>
#include <lib/stdlib.h>

namespace Kernel
{

/* src/rust/tls */
extern "C" void* tls_connect(void* conn, const u8* host, ulong hostLen);
extern "C" long tls_send(void* stream, const u8* buf, ulong len);
extern "C" long tls_recv(void* stream, u8* buf, ulong len);
extern "C" void tls_close(void* stream);

TlsConn::TlsConn()
    : Stream(nullptr)
{
}

TlsConn::~TlsConn()
{
    Close();
}

bool TlsConn::Connect(TcpConn* conn, const char* host)
{
    if (Stream != nullptr || conn == nullptr || host == nullptr)
        return false;

    Stream = tls_connect(conn, (const u8*)host, Stdlib::StrLen(host));
    if (Stream == nullptr)
    {
        Trace(0, "TlsConn: handshake with %s failed", host);
        return false;
    }

    return true;
}

bool TlsConn::Send(const void* data, ulong len)
{
    if (Stream == nullptr)
        return false;

    return tls_send(Stream, (const u8*)data, len) == (long)len;
}

long TlsConn::Recv(void* buf, ulong len)
{
    if (Stream == nullptr)
        return -1;

    return tls_recv(Stream, (u8*)buf, len);
}

void TlsConn::Close()
{
    if (Stream == nullptr)
        return;

    tls_close(Stream);
    Stream = nullptr;
}

} /* namespace Kernel */
