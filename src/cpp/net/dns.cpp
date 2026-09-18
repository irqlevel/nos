#include "dns.h"

#include <net/net_device.h>
#include <lib/stdlib.h>

extern "C" {

int rust_dns_start(unsigned long dev, unsigned int serverIp);
int rust_dns_ready();
int rust_dns_resolve(const char* name, unsigned long len, unsigned long timeoutMs,
    unsigned int* ip);
void rust_dns_flush();
int rust_dns_entry(unsigned long index, char* name, unsigned long cap, unsigned int* ip);

}

namespace Kernel
{

bool DnsResolver::Init(NetDevice* dev, Net::IpAddress dnsServerIp)
{
    if (dev == nullptr)
        return false;

    return rust_dns_start(reinterpret_cast<unsigned long>(dev), dnsServerIp.Addr4) == 0;
}

bool DnsResolver::IsInitialized()
{
    return rust_dns_ready() != 0;
}

bool DnsResolver::Resolve(const char* name, Net::IpAddress& ip, ulong timeoutMs)
{
    if (name == nullptr)
        return false;

    ulong len = Stdlib::StrLen(name);
    u32 addr = 0;
    if (rust_dns_resolve(name, len, timeoutMs, &addr) != 0)
        return false;

    ip.Addr4 = addr;
    return true;
}

void DnsResolver::Flush()
{
    rust_dns_flush();
}

void DnsResolver::Dump(Stdlib::Printer& printer)
{
    printer.Printf("DNS cache:\n");

    char name[MaxDomainLen + 1];
    u32 addr;
    for (ulong i = 0; rust_dns_entry(i, name, sizeof(name), &addr) == 0; i++)
    {
        printer.Printf("  %s -> %u.%u.%u.%u\n", name,
            (ulong)((addr >> 24) & 0xFF), (ulong)((addr >> 16) & 0xFF),
            (ulong)((addr >> 8) & 0xFF), (ulong)(addr & 0xFF));
    }
}

}
