#include "http.h"
#include "tcp.h"
#include "dns.h"
#include <kernel/trace.h>
#include <kernel/time.h>
#include <mm/new.h>
#include <lib/stdlib.h>

namespace Kernel
{

HttpClient::HttpClient(NetDevice* dev)
    : Dev(dev)
{
}

HttpClient::~HttpClient()
{
}

bool HttpClient::ParseUrl(const char* url, char* host, ulong hostSize,
                          u16& port, char* path, ulong pathSize)
{
    /* Expected: http://host[:port][/path] */
    const char* p = url;

    /* Skip scheme */
    static const char httpPrefix[] = "http://";
    static const ulong httpPrefixLen = 7;
    if (Stdlib::StrnCmp(p, httpPrefix, httpPrefixLen) == 0)
        p += httpPrefixLen;

    /* Extract host (and optional port) */
    const char* hostStart = p;
    const char* hostEnd = nullptr;
    port = HttpDefaultPort;

    /* Find end of host: '/', ':', or end of string */
    while (*p && *p != '/' && *p != ':')
        p++;

    hostEnd = p;
    ulong hLen = (ulong)(hostEnd - hostStart);
    if (hLen == 0 || hLen >= hostSize)
        return false;

    Stdlib::MemCpy(host, hostStart, hLen);
    host[hLen] = '\0';

    /* Optional port */
    if (*p == ':')
    {
        p++;
        u32 portVal = 0;
        while (*p >= '0' && *p <= '9')
        {
            portVal = portVal * 10 + (u32)(*p - '0');
            p++;
        }
        if (portVal == 0 || portVal > 65535)
            return false;
        port = (u16)portVal;
    }

    /* Path */
    if (*p == '/')
    {
        ulong pLen = Stdlib::StrLen(p);
        if (pLen >= pathSize)
            pLen = pathSize - 1;
        Stdlib::MemCpy(path, p, pLen);
        path[pLen] = '\0';
    }
    else
    {
        path[0] = '/';
        path[1] = '\0';
    }

    return true;
}

bool HttpClient::ResolveHost(const char* host, Net::IpAddress& ip)
{
    /* Try IP literal first */
    if (Net::IpAddress::Parse(host, ip))
        return true;

    /* DNS resolve */
    if (!DnsResolver::GetInstance().IsInitialized())
        return false;

    return DnsResolver::GetInstance().Resolve(host, ip);
}

bool HttpClient::SendRequest(TcpConn* conn, const char* method,
                             const char* host, const char* path)
{
    /* Build request:
       METHOD /path HTTP/1.1\r\n
       Host: hostname\r\n
       Connection: close\r\n
       \r\n */
    static const ulong MaxReqLen = 512;
    char req[MaxReqLen];
    ulong off = 0;

    const char* parts[] = {
        method, " ", path, " HTTP/1.1\r\nHost: ",
        host, "\r\nConnection: close\r\n\r\n"
    };
    for (ulong i = 0; i < sizeof(parts) / sizeof(parts[0]); i++)
    {
        ulong slen = Stdlib::StrLen(parts[i]);
        /* Fail rather than silently dropping a part: a truncated request line
           or missing Host header would otherwise go out as a plausible but
           wrong request. */
        if (off + slen >= MaxReqLen)
            return false;
        Stdlib::MemCpy(req + off, parts[i], slen);
        off += slen;
    }

    long sent = Tcp::GetInstance().Send(conn, req, off);
    return (sent > 0);
}

/* True when the headers contain "Transfer-Encoding: ... chunked" */
static bool HasChunkedEncoding(const u8* buf, ulong headerLen)
{
    static const char teHeader[] = "Transfer-Encoding:";
    static const ulong teHeaderLen = 18;
    static const char chunked[] = "chunked";
    static const ulong chunkedLen = 7;

    for (ulong j = 0; j + teHeaderLen < headerLen; j++)
    {
        /* Match only at start of a line (j==0 or preceded by \n) */
        if (j != 0 && buf[j - 1] != '\n')
            continue;

        bool match = true;
        for (ulong k = 0; k < teHeaderLen; k++)
        {
            char a = (char)buf[j + k];
            char b = teHeader[k];
            if (a >= 'A' && a <= 'Z') a = a + ('a' - 'A');
            if (b >= 'A' && b <= 'Z') b = b + ('a' - 'A');
            if (a != b) { match = false; break; }
        }
        if (!match)
            continue;

        /* Look for "chunked" anywhere in the header value */
        for (ulong v = j + teHeaderLen;
             v < headerLen && buf[v] != '\r' && buf[v] != '\n'; v++)
        {
            bool m = (v + chunkedLen <= headerLen);
            for (ulong k = 0; m && k < chunkedLen; k++)
            {
                char a = (char)buf[v + k];
                if (a >= 'A' && a <= 'Z') a = a + ('a' - 'A');
                if (a != chunked[k])
                    m = false;
            }
            if (m)
                return true;
        }
        return false;
    }
    return false;
}

/* Incremental chunk-size/CRLF framing (RFC 9112 7.1). The body never
   exists in one piece, so decoding is a state machine over the wire bytes
   as they arrive: Feed() hands the decoded payload to a sink. Trailers
   after the 0-size chunk are ignored; a truncated final chunk keeps the
   bytes that did arrive. */
class HttpChunkDecoder
{
public:
    HttpChunkDecoder()
        : St(StateSize)
        , Remaining(0)
        , SawDigit(false)
    {
    }

    /* False means the sink took less than it was offered -- stop reading. */
    bool Feed(const u8* src, ulong len, HttpSink& out);

    bool IsDone() const { return St == StateDone; }

private:
    enum State
    {
        StateSize,      /* hex chunk size */
        StateExt,       /* ";extension" and the CRLF that ends the size line */
        StateData,      /* Remaining payload bytes */
        StateDataEnd,   /* the CRLF that follows the payload */
        StateDone,      /* the 0-size chunk arrived */
    };

    State St;
    ulong Remaining;
    bool SawDigit;
};

bool HttpChunkDecoder::Feed(const u8* src, ulong len, HttpSink& out)
{
    ulong pos = 0;

    while (pos < len && St != StateDone)
    {
        switch (St)
        {
        case StateSize:
        {
            char c = (char)src[pos];
            ulong digit;
            if (c >= '0' && c <= '9')
                digit = (ulong)(c - '0');
            else if (c >= 'a' && c <= 'f')
                digit = (ulong)(c - 'a') + 10;
            else if (c >= 'A' && c <= 'F')
                digit = (ulong)(c - 'A') + 10;
            else
            {
                /* A size line with no digits at all is framing garbage:
                   stop rather than resynchronize on noise. */
                St = SawDigit ? StateExt : StateDone;
                break;
            }
            Remaining = Remaining * 16 + digit;
            SawDigit = true;
            pos++;
            break;
        }
        case StateExt:
        {
            if (src[pos++] != '\n')
                break;
            SawDigit = false;
            St = (Remaining == 0) ? StateDone : StateData;
            break;
        }
        case StateData:
        {
            ulong chunk = len - pos;
            if (chunk > Remaining)
                chunk = Remaining;
            ulong taken = out.Write(src + pos, chunk);
            pos += taken;
            Remaining -= taken;
            if (taken < chunk)
                return false;
            if (Remaining == 0)
                St = StateDataEnd;
            break;
        }
        case StateDataEnd:
        {
            if (src[pos++] == '\n')
                St = StateSize;
            break;
        }
        case StateDone:
            break;
        }
    }

    return true;
}

/* Everything the framing says about the body, applied on the way to the
   caller's sink: chunked decoding, the Content-Length cut-off and the hard
   size cap. It is itself the sink the decoder writes through, so the cap
   covers decoded output too. */
class HttpBodyWriter : public HttpSink
{
public:
    HttpBodyWriter(HttpSink& sink, bool chunked, ulong contentLength, ulong limit)
        : Sink(sink)
        , Chunked(chunked)
        , ContentLength(contentLength)
        , Limit(limit)
        , Written(0)
        , Overflow(false)
        , Failed(false)
    {
    }

    /* Raw bytes off the wire; false means there is nothing more to read. */
    bool Feed(const u8* data, ulong len);

    /* HttpSink: decoded bytes out, the cap applied. */
    virtual ulong Write(const u8* data, ulong len) override;

    /* True when the body ended where the framing said it would. */
    bool IsComplete() const;

    ulong GetWritten() const { return Written; }
    bool IsOverflow() const { return Overflow; }

private:
    HttpSink& Sink;
    HttpChunkDecoder Dec;
    bool Chunked;
    ulong ContentLength;
    ulong Limit;
    ulong Written;
    bool Overflow;
    bool Failed;
};

ulong HttpBodyWriter::Write(const u8* data, ulong len)
{
    if (len > Limit - Written)
    {
        /* Hand over what still fits under the cap, then stop. */
        Overflow = true;
        len = Limit - Written;
    }

    if (len == 0)
        return 0;

    ulong taken = Sink.Write(data, len);
    Written += taken;
    if (taken < len)
        Failed = true;

    return taken;
}

bool HttpBodyWriter::Feed(const u8* data, ulong len)
{
    if (Chunked)
    {
        if (!Dec.Feed(data, len, *this))
            return false;
        return !Dec.IsDone();
    }

    if (ContentLength != 0)
    {
        /* Content-Length is the authority when it is there: stop on the
           last byte instead of waiting out the peer's FIN. */
        ulong remaining = ContentLength - Written;
        if (len > remaining)
            len = remaining;
        if (Write(data, len) < len)
            return false;
        return Written < ContentLength;
    }

    /* No framing but the close: read until EOF. */
    return Write(data, len) == len;
}

bool HttpBodyWriter::IsComplete() const
{
    if (Overflow || Failed)
        return false;
    if (Chunked)
        return Dec.IsDone();
    if (ContentLength != 0)
        return Written >= ContentLength;
    return true;
}

HttpMemorySink::HttpMemorySink(ulong cap)
    : Buf(nullptr)
    , Cap(cap)
    , Used(0)
{
}

HttpMemorySink::~HttpMemorySink()
{
    if (Buf != nullptr)
        Mm::Free(Buf);
}

ulong HttpMemorySink::Write(const u8* data, ulong len)
{
    if (len == 0)
        return 0;

    /* Allocated on the first byte, so an empty body costs nothing. */
    if (Buf == nullptr)
    {
        Buf = (u8*)Mm::Alloc(Cap, 'Http');
        if (Buf == nullptr)
            return 0;
    }

    /* A body that does not fit keeps its first Cap bytes; the short count
       stops the transfer and marks the response truncated. */
    if (len > Cap - Used)
        len = Cap - Used;

    Stdlib::MemCpy(Buf + Used, data, len);
    Used += len;
    return len;
}

u8* HttpMemorySink::Take()
{
    u8* buf = Buf;
    Buf = nullptr;
    return buf;
}

/* Offset of the "\r\n\r\n" boundary's first byte past it, searching buf
   from `from`; 0 when the headers have not ended yet. */
static ulong FindHeaderEnd(const u8* buf, ulong from, ulong total)
{
    for (ulong j = from; j + 3 < total; j++)
    {
        if (buf[j] == '\r' && buf[j + 1] == '\n' &&
            buf[j + 2] == '\r' && buf[j + 3] == '\n')
            return j + 4;
    }
    return 0;
}

bool HttpClient::RecvResponse(TcpConn* conn, HttpResponse& resp, HttpSink& sink)
{
    /* One buffer for the whole exchange: it holds the headers first, then
       carries the body a receive at a time. A 20 MB download costs no more
       memory than a 200 byte one. */
    u8* buf = (u8*)Mm::Alloc(HttpMaxHeaderSize, 'Http');
    if (!buf)
        return false;

    ulong total = 0;
    ulong searched = 0;
    ulong headerEnd = 0;
    bool eof = false;

    for (;;)
    {
        headerEnd = FindHeaderEnd(buf, searched, total);
        if (headerEnd != 0 || eof)
            break;

        if (total >= HttpMaxHeaderSize)
        {
            Trace(0, "HttpClient: headers larger than %u bytes",
                  (ulong)HttpMaxHeaderSize);
            Mm::Free(buf);
            return false;
        }

        /* A boundary can straddle two receives, so rescan the last 3 bytes. */
        searched = (total >= 3) ? total - 3 : 0;

        long got = Tcp::GetInstance().Recv(conn, buf + total,
                                           HttpMaxHeaderSize - total,
                                           HttpRecvTimeoutMs);
        if (got > 0)
        {
            total += (ulong)got;
        }
        else if (got == 0)
        {
            eof = true;
        }
        else
        {
            Trace(0, "HttpClient: %s waiting for headers",
                  (got == TcpRecvTimeout) ? "timeout" : "receive error");
            Mm::Free(buf);
            return false;
        }
    }

    if (total == 0)
    {
        Mm::Free(buf);
        return false;
    }

    /* Parse status line: HTTP/1.x SSS ... */
    resp.StatusCode = 0;
    ulong i = 0;

    /* Skip "HTTP/1.x " */
    while (i < total && buf[i] != ' ')
        i++;
    if (i < total)
        i++; /* skip space */

    /* Parse status code */
    while (i < total && buf[i] >= '0' && buf[i] <= '9')
    {
        resp.StatusCode = resp.StatusCode * 10 + (int)(buf[i] - '0');
        i++;
    }

    if (headerEnd == 0)
    {
        /* No header boundary before the peer hung up -- treat everything
           that arrived as the body. */
        resp.BodyLen = sink.Write(buf, total);
        resp.ContentLength = resp.BodyLen;
        resp.Truncated = true;
        resp.Ok = true;
        resp.Err = MakeError(Stdlib::Error::UnexpectedEOF);
        Mm::Free(buf);
        return true;
    }

    /* Extract Content-Length from headers if present */
    resp.ContentLength = 0;
    static const char clHeader[] = "Content-Length:";
    static const ulong clHeaderLen = 15;
    for (ulong j = 0; j + clHeaderLen < headerEnd; j++)
    {
        bool match = true;
        for (ulong k = 0; k < clHeaderLen; k++)
        {
            char a = (char)buf[j + k];
            char b = clHeader[k];
            /* Case-insensitive compare */
            if (a >= 'A' && a <= 'Z') a = a + ('a' - 'A');
            if (b >= 'A' && b <= 'Z') b = b + ('a' - 'A');
            if (a != b) { match = false; break; }
        }
        if (match)
        {
            ulong v = j + clHeaderLen;
            while (v < headerEnd && buf[v] == ' ')
                v++;
            while (v < headerEnd && buf[v] >= '0' && buf[v] <= '9')
            {
                resp.ContentLength = resp.ContentLength * 10 +
                                     (ulong)(buf[v] - '0');
                v++;
            }
            break;
        }
    }

    bool chunked = HasChunkedEncoding(buf, headerEnd);

    /* Extract Location header for redirects */
    ExtractLocation(buf, headerEnd, resp.Location, sizeof(resp.Location));

    if (resp.IsRedirect())
    {
        /* The body of a redirect is of no interest to anyone, and the
           connection is closed right after -- do not read it at all. */
        resp.BodyLen = 0;
        resp.Ok = true;
        resp.Err = MakeSuccess();
        Mm::Free(buf);
        return true;
    }

    if (!chunked && resp.ContentLength > HttpMaxBodySize)
    {
        /* Refuse before the transfer rather than after 20 MB of it. */
        Trace(0, "HttpClient: body of %u bytes over the %u byte limit",
              resp.ContentLength, (ulong)HttpMaxBodySize);
        resp.BodyLen = 0;
        resp.Truncated = true;
        resp.Ok = true;
        resp.Err = MakeError(Stdlib::Error::BufTooBig);
        Mm::Free(buf);
        return true;
    }

    HttpBodyWriter writer(sink, chunked, resp.ContentLength, HttpMaxBodySize);

    bool keepReading = writer.Feed(buf + headerEnd, total - headerEnd);
    bool recvFailed = false;

    while (keepReading && !eof)
    {
        long got = Tcp::GetInstance().Recv(conn, buf, HttpMaxHeaderSize,
                                           HttpRecvTimeoutMs);
        if (got > 0)
        {
            keepReading = writer.Feed(buf, (ulong)got);
        }
        else if (got == 0)
        {
            eof = true;
        }
        else
        {
            Trace(0, "HttpClient: %s after %u body bytes",
                  (got == TcpRecvTimeout) ? "timeout" : "receive error",
                  writer.GetWritten());
            recvFailed = true;
            break;
        }
    }

    resp.BodyLen = writer.GetWritten();
    if (resp.ContentLength == 0 || chunked)
        resp.ContentLength = resp.BodyLen;

    resp.Truncated = recvFailed || !writer.IsComplete();
    if (writer.IsOverflow())
        resp.Err = MakeError(Stdlib::Error::BufTooBig);
    else if (resp.Truncated)
        resp.Err = MakeError(Stdlib::Error::UnexpectedEOF);
    else
        resp.Err = MakeSuccess();

    resp.Ok = true;
    Mm::Free(buf);
    return true;
}

void HttpClient::ExtractLocation(const u8* headers, ulong headerLen,
                                 char* loc, ulong locSize)
{
    static const char locHeader[] = "Location:";
    static const ulong locHeaderLen = 9;

    for (ulong j = 0; j + locHeaderLen < headerLen; j++)
    {
        /* Match only at start of a line (j==0 or preceded by \n) */
        if (j != 0 && headers[j - 1] != '\n')
            continue;

        bool match = true;
        for (ulong k = 0; k < locHeaderLen; k++)
        {
            char a = (char)headers[j + k];
            char b = locHeader[k];
            if (a >= 'A' && a <= 'Z') a = a + ('a' - 'A');
            if (b >= 'A' && b <= 'Z') b = b + ('a' - 'A');
            if (a != b) { match = false; break; }
        }
        if (match)
        {
            ulong v = j + locHeaderLen;
            while (v < headerLen && headers[v] == ' ')
                v++;
            ulong start = v;
            while (v < headerLen && headers[v] != '\r' && headers[v] != '\n')
                v++;
            ulong len = v - start;
            if (len >= locSize)
                len = locSize - 1;
            Stdlib::MemCpy(loc, headers + start, len);
            loc[len] = '\0';
            return;
        }
    }
    loc[0] = '\0';
}

HttpResponse HttpClient::DoGet(const char* url, HttpSink& sink)
{
    HttpResponse resp;

    char host[HttpMaxUrlHostLen];
    char path[HttpMaxUrlPathLen];
    u16 port = HttpDefaultPort;

    if (!ParseUrl(url, host, sizeof(host), port, path, sizeof(path)))
    {
        Trace(0, "HttpClient: failed to parse URL");
        return resp;
    }

    /* Resolve host */
    Net::IpAddress ip;
    if (!ResolveHost(host, ip))
    {
        Trace(0, "HttpClient: failed to resolve host");
        return resp;
    }

    /* TCP connect */
    TcpConn* conn = Tcp::GetInstance().Connect(Dev, ip, port);
    if (!conn)
    {
        Trace(0, "HttpClient: TCP connect failed");
        return resp;
    }

    /* Send GET request */
    if (!SendRequest(conn, "GET", host, path))
    {
        Trace(0, "HttpClient: failed to send request");
        Tcp::GetInstance().Close(conn);
        return resp;
    }

    /* Receive response */
    if (!RecvResponse(conn, resp, sink))
    {
        Trace(0, "HttpClient: failed to receive response");
        Tcp::GetInstance().Close(conn);
        return resp;
    }

    Tcp::GetInstance().Close(conn);
    return resp;
}

HttpResponse HttpClient::Get(const char* url)
{
    HttpMemorySink sink(HttpMaxResponseSize);
    HttpResponse resp = Get(url, sink);
    resp.Body = sink.Take();
    return resp;
}

HttpResponse HttpClient::Get(const char* url, HttpSink& sink)
{
    char currentUrl[HttpMaxUrlHostLen + HttpMaxUrlPathLen];
    ulong urlLen = Stdlib::StrLen(url);
    if (urlLen >= sizeof(currentUrl))
        urlLen = sizeof(currentUrl) - 1;
    Stdlib::MemCpy(currentUrl, url, urlLen);
    currentUrl[urlLen] = '\0';

    for (ulong attempt = 0; attempt <= HttpMaxRedirects; attempt++)
    {
        HttpResponse resp = DoGet(currentUrl, sink);

        if (!resp.Ok || !resp.IsRedirect())
            return resp;

        /* Only follow http:// redirects */
        static const char httpPrefix[] = "http://";
        static const ulong httpPrefixLen = 7;
        if (Stdlib::StrnCmp(resp.Location, httpPrefix, httpPrefixLen) != 0)
        {
            Trace(0, "HttpClient: %u redirect to non-HTTP: %s",
                  (ulong)resp.StatusCode, resp.Location);
            return resp;
        }

        Trace(0, "HttpClient: %u redirect -> %s", (ulong)resp.StatusCode, resp.Location);

        /* The body of a redirect never reached the sink (RecvResponse drops
           it), so there is nothing to release here. */

        urlLen = Stdlib::StrLen(resp.Location);
        if (urlLen >= sizeof(currentUrl))
            urlLen = sizeof(currentUrl) - 1;
        Stdlib::MemCpy(currentUrl, resp.Location, urlLen);
        currentUrl[urlLen] = '\0';
    }

    Trace(0, "HttpClient: too many redirects");
    HttpResponse fail;
    return fail;
}

} /* namespace Kernel */
