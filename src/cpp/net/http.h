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
static const ulong HttpMaxUrlHostLen = 128;
static const ulong HttpMaxUrlPathLen = 256;
static const ulong HttpMaxLocationLen = 256;
static const ulong HttpRecvTimeoutMs = 10000;
static const ulong HttpMaxRedirects = 5;

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
    char Location[HttpMaxLocationLen]; /* redirect target from Location header */
    bool Ok;
    bool Truncated;     /* body cut short: size cap, sink refusal, an idle
                           timeout or a peer that closed early */
    Stdlib::Error Err;

    HttpResponse()
        : StatusCode(0)
        , ContentLength(0)
        , Body(nullptr)
        , BodyLen(0)
        , Ok(false)
        , Truncated(false)
        , Err(MakeError(Stdlib::Error::InvalidState))
    {
        Location[0] = '\0';
    }

    bool IsRedirect() const
    {
        return (StatusCode == 301 || StatusCode == 302 ||
                StatusCode == 303 || StatusCode == 307 ||
                StatusCode == 308) && Location[0] != '\0';
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
    NetDevice* Dev;

    HttpResponse DoGet(const char* url, HttpSink& sink);
    bool ParseUrl(const char* url, char* host, ulong hostSize,
                  u16& port, char* path, ulong pathSize);
    bool ResolveHost(const char* host, Net::IpAddress& ip);
    bool SendRequest(TcpConn* conn, const char* method,
                     const char* host, const char* path);
    bool RecvResponse(TcpConn* conn, HttpResponse& resp, HttpSink& sink);
    void ExtractLocation(const u8* headers, ulong headerLen, char* loc, ulong locSize);
};

} /* namespace Kernel */
