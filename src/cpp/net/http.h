#pragma once

#include <include/types.h>
#include <lib/error.h>
#include <net/net.h>
#include <net/net_device.h>

namespace Kernel
{

struct TcpConn;

/* HTTP client constants */
/* Cap on a body kept in memory. The heap cannot serve a single allocation
   larger than 512 KB anyway (PageTable::MaxContiguousPages), so anything
   bigger has to stream -- see HttpSink and HttpMaxBodySize. */
static const ulong HttpMaxResponseSize = 32768;
/* Cap on a streamed body. 20 MB is the largest download the shell offers. */
static const ulong HttpMaxBodySize = 20 * 1024 * 1024;
/* Status line + headers must fit here; the same buffer then carries the
   body in receive-sized pieces. */
static const ulong HttpMaxHeaderSize = 16384;
static const u16   HttpDefaultPort = 80;
static const u16   HttpsDefaultPort = 443;
static const ulong HttpMaxUrlHostLen = 128;
/* A whole URL, path and query included. URLs run long: a GitHub release
   download redirects to a signed link whose query carries a signature and a
   JWT, ~900 characters of it. 2 KiB is the long-standing practical limit
   servers and browsers accept. A URL that does not fit fails the request
   instead of being cut short: a truncated URL is a different URL -- a
   signed link minus its signature -- and whether the server still answers
   it depends on what its cache happens to hold. */
static const ulong HttpMaxUrlLen = 2048;
/* The request: method, path, the Host header and the framing around them. */
static const ulong HttpMaxRequestLen = HttpMaxUrlLen + HttpMaxUrlHostLen + 64;
static const ulong HttpRecvTimeoutMs = 10000;
static const ulong HttpMaxRedirects = 5;

/* The byte pipe the exchange runs over: plain TCP, or TLS on top of it.
   Everything above it -- the request, the header parsing, chunked decoding,
   redirects and the body sink -- is the same either way. */
class HttpTransport
{
public:
    virtual ~HttpTransport() {}
    virtual bool Send(const void* data, ulong len) = 0;
    /* Bytes read, 0 at end of stream, negative on error or timeout. The
       timeout is advisory: the TLS transport uses its own. */
    virtual long Recv(void* buf, ulong len, ulong timeoutMs) = 0;
};

/* Where a response body goes as it arrives. The client never holds a whole
   body: a 20 MB download passes through Write() in receive-buffer sized
   pieces, so the peak cost of any transfer is one HttpMaxHeaderSize buffer
   plus whatever the sink itself keeps.

   Write returns how many of the len bytes it took. A short count ends the
   transfer and marks the response truncated -- and, because it is a count
   and not a flag, the response's BodyLen is exactly what the sink holds. */
class HttpSink
{
public:
    virtual ~HttpSink() {}
    virtual ulong Write(const u8* data, ulong len) = 0;
};

/* Collects a body into one heap buffer, at most Cap bytes; a body that does
   not fit stops the transfer rather than growing the buffer. */
class HttpMemorySink : public HttpSink
{
public:
    HttpMemorySink(ulong cap);
    virtual ~HttpMemorySink();

    virtual ulong Write(const u8* data, ulong len) override;

    /* Hands the buffer to the caller, who must Mm::Free it. */
    u8* Take();
    ulong GetLen() const { return Used; }

private:
    HttpMemorySink(const HttpMemorySink& other) = delete;
    HttpMemorySink& operator=(const HttpMemorySink& other) = delete;

    u8* Buf;
    ulong Cap;
    ulong Used;
};

/* HTTP response */
struct HttpResponse
{
    int StatusCode;
    ulong ContentLength;
    u8* Body;           /* memory mode only: heap-allocated via Mm::Alloc,
                           caller must Mm::Free. Null when a sink took the
                           body. */
    ulong BodyLen;      /* bytes delivered, to memory or to the sink */
    char Location[HttpMaxUrlLen]; /* redirect target from Location header */
    bool Ok;
    bool Truncated;     /* body cut short: size cap, sink refusal, an idle
                           timeout or a peer that closed early */
    bool TlsFailed;     /* the TLS handshake was refused -- a certificate
                           that did not verify, or no common protocol */
    Stdlib::Error Err;  /* BufTooBig with Ok false: the URL, or a redirect's
                           target, is longer than HttpMaxUrlLen */

    HttpResponse()
    {
        Reset();
    }

    static bool IsRedirectStatus(int code)
    {
        return code == 301 || code == 302 || code == 303 ||
               code == 307 || code == 308;
    }

    bool IsRedirect() const
    {
        return IsRedirectStatus(StatusCode) && Location[0] != '\0';
    }

    void Reset()
    {
        StatusCode = 0;
        ContentLength = 0;
        Body = nullptr;
        BodyLen = 0;
        Ok = false;
        Truncated = false;
        TlsFailed = false;
        Err = MakeError(Stdlib::Error::InvalidState);
        Location[0] = '\0';
    }
};

class HttpClient
{
public:
    HttpClient(NetDevice* dev);
    ~HttpClient();

    /* HTTP GET -- connects, sends request, receives response, closes.
       The body of the final response (redirects are followed and their
       bodies dropped) goes to the sink; the no-sink form keeps it in
       resp.Body, capped at HttpMaxResponseSize. */
    HttpResponse Get(const char* url);
    HttpResponse Get(const char* url, HttpSink& sink);

private:
    struct Exchange;

    NetDevice* Dev;

    void DoGet(Exchange& ex, HttpSink& sink, HttpResponse& resp);
    bool ParseUrl(const char* url, char* host, ulong hostSize,
                  u16& port, char* path, ulong pathSize, bool& tls);
    bool ResolveHost(const char* host, Net::IpAddress& ip);
    bool SendRequest(HttpTransport& transport, const char* method, Exchange& ex);
    bool RecvResponse(HttpTransport& transport, HttpResponse& resp, HttpSink& sink);
    bool ExtractLocation(const u8* headers, ulong headerLen, char* loc, ulong locSize);
};

} /* namespace Kernel */
