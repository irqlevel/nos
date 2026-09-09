#pragma once

#include <include/types.h>

namespace Kernel
{

struct TcpConn;

/* A TLS session over a TCP connection.
 *
 * The protocol itself lives in Rust (src/rust/tls: rustls, RustCrypto and
 * the Mozilla root store), reached through the four calls below. This class
 * is the C++ half: it owns the session handle, not the connection -- the
 * caller opened the TcpConn and closes it.
 *
 * Certificates are verified against the built-in roots and the wall clock,
 * so a machine whose RTC is wrong rejects every server it meets. */
class TlsConn
{
public:
    TlsConn();
    ~TlsConn();

    /* Runs the handshake on an established connection. `host` is both the
       SNI name and the name the certificate must match. */
    bool Connect(TcpConn* conn, const char* host);

    /* Encrypts and sends the whole buffer; false if the session failed. */
    bool Send(const void* data, ulong len);

    /* Decrypted bytes, 0 at end of stream, negative on error. */
    long Recv(void* buf, ulong len);

    /* Sends close_notify and releases the session; the TCP connection
       stays open for its owner to close. */
    void Close();

    bool IsOpen() const { return Stream != nullptr; }

private:
    TlsConn(const TlsConn& other) = delete;
    TlsConn& operator=(const TlsConn& other) = delete;

    void* Stream;
};

} /* namespace Kernel */
