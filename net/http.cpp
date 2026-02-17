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
        if (off + slen < MaxReqLen)
        {
            Stdlib::MemCpy(req + off, parts[i], slen);
            off += slen;
        }
    }

    long sent = Tcp::GetInstance().Send(conn, req, off);
    return (sent > 0);
}

bool HttpClient::RecvResponse(TcpConn* conn, HttpResponse& resp)
{
    /* Receive into a temp buffer */
    u8* buf = (u8*)Mm::Alloc(HttpMaxResponseSize, 'Http');
    if (!buf)
        return false;

    ulong total = 0;
    Stdlib::Time bt = GetBootTime();
    ulong deadline = bt.GetSecs() * 1000 + bt.GetUsecs() / 1000 + HttpRecvTimeoutMs;

    while (total < HttpMaxResponseSize)
    {
        long got = Tcp::GetInstance().Recv(conn, buf + total,
                                           HttpMaxResponseSize - total);
        if (got > 0)
        {
            total += (ulong)got;
            bt = GetBootTime();
            deadline = bt.GetSecs() * 1000 + bt.GetUsecs() / 1000 + HttpRecvTimeoutMs;
        }
        else if (got == 0)
        {
            break; /* EOF */
        }
        else
        {
            Mm::Free(buf);
            return false;
        }

        bt = GetBootTime();
        if (bt.GetSecs() * 1000 + bt.GetUsecs() / 1000 > deadline)
            break;
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

    /* Find header/body boundary: \r\n\r\n */
    ulong headerEnd = 0;
    for (ulong j = 0; j + 3 < total; j++)
    {
        if (buf[j] == '\r' && buf[j + 1] == '\n' &&
            buf[j + 2] == '\r' && buf[j + 3] == '\n')
        {
            headerEnd = j + 4;
            break;
        }
    }

    if (headerEnd == 0)
    {
        /* No header boundary found -- treat entire response as body */
        resp.Body = buf;
        resp.BodyLen = total;
        resp.ContentLength = total;
        resp.Ok = true;
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

    /* Body starts after header boundary */
    ulong bodyLen = total - headerEnd;
    if (bodyLen > 0)
    {
        resp.Body = (u8*)Mm::Alloc(bodyLen, 'Http');
        if (resp.Body)
        {
            Stdlib::MemCpy(resp.Body, buf + headerEnd, bodyLen);
            resp.BodyLen = bodyLen;
        }
    }
    if (resp.ContentLength == 0)
        resp.ContentLength = bodyLen;

    /* Extract Location header for redirects */
    ExtractLocation(buf, headerEnd, resp.Location, sizeof(resp.Location));

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

HttpResponse HttpClient::DoGet(const char* url)
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
    if (!RecvResponse(conn, resp))
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
    char currentUrl[HttpMaxUrlHostLen + HttpMaxUrlPathLen];
    ulong urlLen = Stdlib::StrLen(url);
    if (urlLen >= sizeof(currentUrl))
        urlLen = sizeof(currentUrl) - 1;
    Stdlib::MemCpy(currentUrl, url, urlLen);
    currentUrl[urlLen] = '\0';

    for (ulong attempt = 0; attempt <= HttpMaxRedirects; attempt++)
    {
        HttpResponse resp = DoGet(currentUrl);

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

        /* Free body from redirect response */
        if (resp.Body)
        {
            Mm::Free(resp.Body);
            resp.Body = nullptr;
            resp.BodyLen = 0;
        }

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
