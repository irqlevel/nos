#include "http.h"

#include <net/net_device.h>
#include <kernel/trace.h>
#include <mm/new.h>
#include <lib/stdlib.h>

extern "C" {

/* What a GET turned out to be; crate::http::Response is the same struct. */
struct RustHttpResponse
{
    int Status;
    unsigned long ContentLength;
    unsigned long BodyLen;
    unsigned int Ok;
    unsigned int Truncated;
    unsigned int TlsFailed;
    unsigned int UrlTooLong;
};

int rust_http_get(unsigned long dev, const unsigned char* url, unsigned long urlLen,
    unsigned long (*sink)(void* ctx, const unsigned char* data, unsigned long len),
    void* ctx, RustHttpResponse* out, char* location, unsigned long locationCap);

}

namespace Kernel
{

/* The sink the client writes through: a C++ HttpSink, reached from Rust by
   this one function. */
static unsigned long SinkWrite(void* ctx, const unsigned char* data,
    unsigned long len)
{
    return static_cast<HttpSink*>(ctx)->Write(data, len);
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

HttpResponse HttpClient::Get(const char* url, HttpSink& sink)
{
    HttpResponse resp;

    if (Dev == nullptr || url == nullptr)
        return resp;

    ulong urlLen = Stdlib::StrLen(url);
    if (urlLen == 0 || urlLen >= HttpMaxUrlLen)
    {
        Trace(0, "HttpClient: URL longer than %u characters", HttpMaxUrlLen - 1);
        resp.Err = MakeError(Stdlib::Error::BufTooBig);
        return resp;
    }

    RustHttpResponse out = {};
    if (rust_http_get(reinterpret_cast<unsigned long>(Dev),
            (const unsigned char*)url, urlLen, SinkWrite, &sink, &out,
            resp.Location, sizeof(resp.Location)) != 0)
    {
        return resp;
    }

    resp.StatusCode = out.Status;
    resp.ContentLength = out.ContentLength;
    resp.BodyLen = out.BodyLen;
    resp.Ok = (out.Ok != 0);
    resp.Truncated = (out.Truncated != 0);
    resp.TlsFailed = (out.TlsFailed != 0);

    if (out.UrlTooLong)
        resp.Err = MakeError(Stdlib::Error::BufTooBig);
    else if (resp.Truncated)
        resp.Err = MakeError(Stdlib::Error::UnexpectedEOF);
    else if (resp.Ok)
        resp.Err = MakeSuccess();

    return resp;
}

HttpResponse HttpClient::Get(const char* url)
{
    HttpMemorySink sink(HttpMaxResponseSize);
    HttpResponse resp = Get(url, sink);
    resp.Body = sink.Take();
    return resp;
}

} /* namespace Kernel */
