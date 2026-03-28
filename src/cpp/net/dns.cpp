#include "dns.h"
#include "net.h"

#include <kernel/trace.h>
#include <kernel/sched.h>
#include <kernel/time.h>
#include <lib/stdlib.h>
#include <include/const.h>

namespace Kernel
{

using Net::EthHdr;
using Net::IpHdr;
using Net::UdpHdr;
using Net::IpAddress;
using Net::Htons;
using Net::Ntohs;

DnsResolver::DnsResolver()
    : Dev(nullptr)
    , NextId(1)
    , Ready(false)
{
    Stdlib::MemSet(&Pending, 0, sizeof(Pending));
    for (ulong i = 0; i < CacheSize; i++)
        Cache[i].Valid = false;
}

DnsResolver::~DnsResolver()
{
}

bool DnsResolver::Init(NetDevice* dev, Net::IpAddress dnsServerIp)
{
    if (!dev || dnsServerIp.IsZero())
        return false;

    Dev = dev;
    ServerIp = dnsServerIp;

    if (!Dev->RegisterUdpListener(ClientPort, RxCallback, this))
    {
        Trace(0, "DNS: failed to register UDP listener on port %u", (ulong)ClientPort);
        return false;
    }

    Ready = true;

    Trace(0, "DNS: resolver started, server %u.%u.%u.%u",
        (ulong)((ServerIp.Addr4 >> 24) & 0xFF),
        (ulong)((ServerIp.Addr4 >> 16) & 0xFF),
        (ulong)((ServerIp.Addr4 >> 8) & 0xFF),
        (ulong)(ServerIp.Addr4 & 0xFF));

    return true;
}

bool DnsResolver::IsInitialized()
{
    return Ready;
}

bool DnsResolver::Lookup(const char* name, Net::IpAddress& ip)
{
    Stdlib::AutoLock lock(CacheLock);
    for (ulong i = 0; i < CacheSize; i++)
    {
        if (Cache[i].Valid && Stdlib::StrCmp(Cache[i].Name, name) == 0)
        {
            ip = Cache[i].Ip;
            return true;
        }
    }
    return false;
}

void DnsResolver::Insert(const char* name, Net::IpAddress ip)
{
    Stdlib::AutoLock lock(CacheLock);

    /* Check if already cached */
    for (ulong i = 0; i < CacheSize; i++)
    {
        if (Cache[i].Valid && Stdlib::StrCmp(Cache[i].Name, name) == 0)
        {
            Cache[i].Ip = ip;
            return;
        }
    }

    /* Find empty slot */
    for (ulong i = 0; i < CacheSize; i++)
    {
        if (!Cache[i].Valid)
        {
            Stdlib::StrnCpy(Cache[i].Name, name, MaxDomainLen);
            Cache[i].Name[MaxDomainLen] = '\0';
            Cache[i].Ip = ip;
            Cache[i].Valid = true;
            return;
        }
    }

    /* Evict slot 0 (simple strategy) */
    Stdlib::StrnCpy(Cache[0].Name, name, MaxDomainLen);
    Cache[0].Name[MaxDomainLen] = '\0';
    Cache[0].Ip = ip;
    Cache[0].Valid = true;
}

void DnsResolver::Flush()
{
    Stdlib::AutoLock lock(CacheLock);
    for (ulong i = 0; i < CacheSize; i++)
        Cache[i].Valid = false;
}

void DnsResolver::Dump(Stdlib::Printer& printer)
{
    Stdlib::AutoLock lock(CacheLock);
    printer.Printf("DNS cache:\n");
    for (ulong i = 0; i < CacheSize; i++)
    {
        if (Cache[i].Valid)
        {
            printer.Printf("  %s -> %u.%u.%u.%u\n",
                Cache[i].Name,
                (ulong)((Cache[i].Ip.Addr4 >> 24) & 0xFF),
                (ulong)((Cache[i].Ip.Addr4 >> 16) & 0xFF),
                (ulong)((Cache[i].Ip.Addr4 >> 8) & 0xFF),
                (ulong)(Cache[i].Ip.Addr4 & 0xFF));
        }
    }
}

ulong DnsResolver::EncodeName(const char* name, u8* buf, ulong bufLen)
{
    ulong nameLen = Stdlib::StrLen(name);
    if (nameLen == 0 || nameLen > MaxDomainLen)
        return 0;

    /* Encoded form: \3www\7example\3com\0
     * Worst case: nameLen + 2 bytes (leading length + trailing zero) */
    if (bufLen < nameLen + 2)
        return 0;

    ulong pos = 0;
    ulong labelStart = 0;

    for (ulong i = 0; i <= nameLen; i++)
    {
        if (i == nameLen || name[i] == '.')
        {
            ulong labelLen = i - labelStart;
            if (labelLen == 0 || labelLen > 63)
                return 0;

            buf[pos++] = (u8)labelLen;
            for (ulong j = labelStart; j < i; j++)
                buf[pos++] = (u8)name[j];

            labelStart = i + 1;
        }
    }

    buf[pos++] = 0; /* root label */
    return pos;
}

ulong DnsResolver::SkipName(const u8* pkt, ulong pktLen, ulong offset)
{
    ulong pos = offset;

    while (pos < pktLen)
    {
        u8 len = pkt[pos];

        if (len == 0)
        {
            return pos + 1; /* skip the zero byte */
        }

        if ((len & CompressFlag) == CompressFlag)
        {
            /* Compression pointer: 2 bytes total */
            if (pos + 1 >= pktLen)
                return 0;
            return pos + 2;
        }

        /* Regular label */
        pos += 1 + len;
    }

    return 0; /* malformed */
}

bool DnsResolver::SendQuery(const char* name, u16 id)
{
    u8 pkt[MaxPacketLen];
    Stdlib::MemSet(pkt, 0, sizeof(pkt));

    /* DNS header */
    DnsHeader* hdr = (DnsHeader*)pkt;
    hdr->Id = Htons(id);
    hdr->Flags = Htons(FlagRD);
    hdr->QdCount = Htons(1);
    hdr->AnCount = 0;
    hdr->NsCount = 0;
    hdr->ArCount = 0;

    ulong off = sizeof(DnsHeader);

    /* Question: encoded name + QTYPE + QCLASS */
    ulong nameLen = EncodeName(name, pkt + off, sizeof(pkt) - off);
    if (nameLen == 0)
        return false;

    off += nameLen;

    if (off + 4 > sizeof(pkt))
        return false;

    /* QTYPE = A */
    pkt[off++] = (u8)(TypeA >> 8);
    pkt[off++] = (u8)(TypeA & 0xFF);
    /* QCLASS = IN */
    pkt[off++] = (u8)(ClassIN >> 8);
    pkt[off++] = (u8)(ClassIN & 0xFF);

    return Dev->SendUdp(ServerIp, ServerPort, Dev->GetIp(), ClientPort, pkt, off);
}

void DnsResolver::RxCallback(const u8* frame, ulong len, void* ctx)
{
    DnsResolver* self = static_cast<DnsResolver*>(ctx);

    static const ulong MinFrameLen = sizeof(EthHdr) + sizeof(IpHdr) + sizeof(UdpHdr) + sizeof(DnsHeader);
    if (len < MinFrameLen)
        return;

    const u8* dnsPayload = frame + sizeof(EthHdr) + sizeof(IpHdr) + sizeof(UdpHdr);
    ulong dnsLen = len - sizeof(EthHdr) - sizeof(IpHdr) - sizeof(UdpHdr);

    self->ProcessResponse(dnsPayload, dnsLen);
}

void DnsResolver::ProcessResponse(const u8* dnsPayload, ulong dnsLen)
{
    if (dnsLen < sizeof(DnsHeader))
        return;

    const DnsHeader* hdr = (const DnsHeader*)dnsPayload;

    u16 id = Ntohs(hdr->Id);
    u16 flags = Ntohs(hdr->Flags);
    u16 qdCount = Ntohs(hdr->QdCount);
    u16 anCount = Ntohs(hdr->AnCount);

    /* Check QR bit (response) */
    if (!(flags & FlagQR))
        return;

    /* Check RCODE == 0 (no error) */
    if ((flags & RcodeMask) != 0)
    {
        Trace(0, "DNS: response rcode %u for id %u", (ulong)(flags & RcodeMask), (ulong)id);
        return;
    }

    /* Check ID matches pending query */
    if (id != Pending.Id)
        return;

    if (anCount == 0)
        return;

    /* Skip question section */
    ulong off = sizeof(DnsHeader);
    for (u16 q = 0; q < qdCount; q++)
    {
        off = SkipName(dnsPayload, dnsLen, off);
        if (off == 0)
            return;
        off += 4; /* QTYPE + QCLASS */
        if (off > dnsLen)
            return;
    }

    /* Parse answer section -- find first A record */
    for (u16 a = 0; a < anCount; a++)
    {
        off = SkipName(dnsPayload, dnsLen, off);
        if (off == 0)
            return;

        /* Need at least Type(2) + Class(2) + TTL(4) + RdLength(2) = 10 bytes */
        if (off + 10 > dnsLen)
            return;

        u16 rType = (u16)((dnsPayload[off] << 8) | dnsPayload[off + 1]);
        u16 rClass = (u16)((dnsPayload[off + 2] << 8) | dnsPayload[off + 3]);
        /* TTL at off+4..off+7, skip */
        u16 rdLength = (u16)((dnsPayload[off + 8] << 8) | dnsPayload[off + 9]);
        off += 10;

        if (off + rdLength > dnsLen)
            return;

        if (rType == TypeA && rClass == ClassIN && rdLength == ARecordLen)
        {
            u32 ipNet;
            Stdlib::MemCpy(&ipNet, dnsPayload + off, 4);
            Pending.Result = IpAddress::FromNetwork(ipNet);
            Pending.Answered = true;
            return;
        }

        off += rdLength;
    }
}

bool DnsResolver::Resolve(const char* name, Net::IpAddress& ip, ulong timeoutMs)
{
    if (!Ready)
        return false;

    if (!name || Stdlib::StrLen(name) == 0 || Stdlib::StrLen(name) > MaxDomainLen)
        return false;

    /* Check cache first (fast path, no mutex) */
    if (Lookup(name, ip))
        return true;

    /* Serialize concurrent Resolve() calls */
    ResolveLock.Lock();

    /* Double-check cache (another caller may have resolved it) */
    if (Lookup(name, ip))
    {
        ResolveLock.Unlock();
        return true;
    }

    u16 id = NextId++;

    Pending.Id = id;
    Pending.Answered = false;

    if (!SendQuery(name, id))
    {
        Trace(0, "DNS: failed to send query for %s", name);
        ResolveLock.Unlock();
        return false;
    }

    Trace(0, "DNS: resolving %s (id %u)", name, (ulong)id);

    ulong elapsed = 0;
    while (elapsed < timeoutMs)
    {
        Sleep(PollIntervalMs * Const::NanoSecsInMs);
        elapsed += PollIntervalMs;

        if (Pending.Answered)
        {
            ip = Pending.Result;
            Insert(name, ip);
            Trace(0, "DNS: resolved %s -> %u.%u.%u.%u",
                name,
                (ulong)((ip.Addr4 >> 24) & 0xFF),
                (ulong)((ip.Addr4 >> 16) & 0xFF),
                (ulong)((ip.Addr4 >> 8) & 0xFF),
                (ulong)(ip.Addr4 & 0xFF));
            ResolveLock.Unlock();
            return true;
        }
    }

    Trace(0, "DNS: timeout resolving %s", name);
    ResolveLock.Unlock();
    return false;
}

}
