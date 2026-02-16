#pragma once

#include <include/types.h>
#include <net/net_device.h>
#include <net/net.h>
#include <kernel/spin_lock.h>
#include <kernel/mutex.h>
#include <lib/printer.h>

namespace Kernel
{

struct DnsHeader
{
    u16 Id;
    u16 Flags;
    u16 QdCount;
    u16 AnCount;
    u16 NsCount;
    u16 ArCount;
} __attribute__((packed));

static_assert(sizeof(DnsHeader) == 12, "Invalid size");

class DnsResolver
{
public:
    static DnsResolver& GetInstance()
    {
        static DnsResolver instance;
        return instance;
    }

    bool Init(NetDevice* dev, Net::IpAddress dnsServerIp);

    /* Resolve hostname to IPv4. Blocks up to timeoutMs. */
    bool Resolve(const char* name, Net::IpAddress& ip, ulong timeoutMs = DefaultTimeoutMs);

    bool IsInitialized();

    void Flush();
    void Dump(Stdlib::Printer& printer);

    /* DNS flag/field constants (host byte order) */
    static const u16 FlagRD       = 0x0100; /* Recursion Desired (bit 8) */
    static const u16 FlagQR       = 0x8000; /* Query/Response (bit 15) */
    static const u16 RcodeMask    = 0x000F;
    static const u16 TypeA        = 1;      /* A record (IPv4) */
    static const u16 ClassIN      = 1;      /* Internet class */
    static const u16 ServerPort   = 53;
    static const u16 ClientPort   = 10053;  /* fixed ephemeral source port */
    static const ulong DefaultTimeoutMs = 3000;
    static const ulong PollIntervalMs   = 10;
    static const ulong MaxDomainLen     = 253;
    static const u8  CompressFlag = 0xC0;   /* DNS name compression pointer */
    static const ulong ARecordLen = 4;      /* IPv4 address size */

private:
    DnsResolver();
    ~DnsResolver();
    DnsResolver(const DnsResolver& other) = delete;
    DnsResolver(DnsResolver&& other) = delete;
    DnsResolver& operator=(const DnsResolver& other) = delete;
    DnsResolver& operator=(DnsResolver&& other) = delete;

    static const ulong CacheSize = 32;

    struct CacheEntry
    {
        char Name[MaxDomainLen + 1];
        Net::IpAddress Ip;
        bool Valid;
    };

    CacheEntry Cache[CacheSize];
    SpinLock CacheLock;
    Mutex ResolveLock;

    struct PendingQuery
    {
        u16 Id;
        bool Answered;
        Net::IpAddress Result;
    };

    PendingQuery Pending;

    NetDevice* Dev;
    Net::IpAddress ServerIp;
    u16 NextId;
    bool Ready;

    bool SendQuery(const char* name, u16 id);
    bool Lookup(const char* name, Net::IpAddress& ip);
    void Insert(const char* name, Net::IpAddress ip);
    static void RxCallback(const u8* frame, ulong len, void* ctx);
    void ProcessResponse(const u8* dnsPayload, ulong dnsLen);

    static ulong EncodeName(const char* name, u8* buf, ulong bufLen);
    static ulong SkipName(const u8* pkt, ulong pktLen, ulong offset);

    static const ulong MaxPacketLen = 512;
};

}
