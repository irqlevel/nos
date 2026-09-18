#pragma once

#include <include/types.h>
#include <net/net.h>
#include <lib/printer.h>

namespace Kernel
{

struct NetDevice;

/* The resolver itself is Rust (src/rust/net/src/dns.rs): the queries, the
   cache and its TTLs, and the checks that keep an off-path answer out. What
   is left here is the way in. */
class DnsResolver
{
public:
    static DnsResolver& GetInstance()
    {
        static DnsResolver instance;
        return instance;
    }

    /* Start resolving through dnsServerIp on dev. */
    bool Init(NetDevice* dev, Net::IpAddress dnsServerIp);
    bool IsInitialized();

    /* Resolve a name to an IPv4 address. Blocks up to timeoutMs. */
    bool Resolve(const char* name, Net::IpAddress& ip, ulong timeoutMs = DefaultTimeoutMs);

    void Flush();
    void Dump(Stdlib::Printer& printer);

    static const ulong DefaultTimeoutMs = 3000;
    static const ulong MaxDomainLen = 253;

private:
    DnsResolver() {}
    ~DnsResolver() {}
    DnsResolver(const DnsResolver& other) = delete;
    DnsResolver(DnsResolver&& other) = delete;
    DnsResolver& operator=(const DnsResolver& other) = delete;
    DnsResolver& operator=(DnsResolver&& other) = delete;
};

}
