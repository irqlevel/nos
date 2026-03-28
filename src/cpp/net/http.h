#pragma once

#include <include/types.h>
#include <lib/error.h>
#include <net/net.h>
#include <net/net_device.h>

namespace Kernel
{

struct TcpConn;

/* HTTP client constants */
static const ulong HttpMaxResponseSize = 32768;
static const u16   HttpDefaultPort = 80;
static const ulong HttpMaxUrlHostLen = 128;
static const ulong HttpMaxUrlPathLen = 256;
static const ulong HttpMaxLocationLen = 256;
static const ulong HttpRecvTimeoutMs = 10000;
static const ulong HttpMaxRedirects = 5;

/* HTTP response */
struct HttpResponse
{
    int StatusCode;
    ulong ContentLength;
    u8* Body;           /* heap-allocated via Mm::Alloc; caller must Mm::Free */
    ulong BodyLen;
    char Location[HttpMaxLocationLen]; /* redirect target from Location header */
    bool Ok;
    Stdlib::Error Err;

    HttpResponse()
        : StatusCode(0)
        , ContentLength(0)
        , Body(nullptr)
        , BodyLen(0)
        , Ok(false)
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

    /* HTTP GET -- connects, sends request, receives response, closes */
    HttpResponse Get(const char* url);

private:
    NetDevice* Dev;

    HttpResponse DoGet(const char* url);
    bool ParseUrl(const char* url, char* host, ulong hostSize,
                  u16& port, char* path, ulong pathSize);
    bool ResolveHost(const char* host, Net::IpAddress& ip);
    bool SendRequest(TcpConn* conn, const char* method,
                     const char* host, const char* path);
    bool RecvResponse(TcpConn* conn, HttpResponse& resp);
    void ExtractLocation(const u8* headers, ulong headerLen, char* loc, ulong locSize);
};

} /* namespace Kernel */
