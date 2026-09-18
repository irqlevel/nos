#include "arp.h"

#include <net/net_device.h>

extern "C" {

void rust_arp_process(unsigned long dev, const unsigned char* data, unsigned long len);
int rust_arp_resolve(unsigned long dev, unsigned int ip, unsigned char* mac);
int rust_arp_lookup(unsigned int ip, unsigned char* mac);
unsigned long rust_arp_snapshot(unsigned int* ips, unsigned char* macs, unsigned long max);

}

namespace Kernel
{

bool ArpTable::Lookup(Net::IpAddress ip, Net::MacAddress& mac)
{
    u8 bytes[6];
    if (rust_arp_lookup(ip.Addr4, bytes) != 0)
        return false;

    mac = Net::MacAddress(bytes);
    return true;
}

bool ArpTable::Resolve(NetDevice* dev, Net::IpAddress ip, Net::MacAddress& mac)
{
    if (dev == nullptr)
        return false;

    u8 bytes[6];
    if (rust_arp_resolve(reinterpret_cast<unsigned long>(dev), ip.Addr4, bytes) != 0)
        return false;

    mac = Net::MacAddress(bytes);
    return true;
}

void ArpTable::Process(NetDevice* dev, const u8* frame, ulong len)
{
    if (dev == nullptr || frame == nullptr)
        return;

    rust_arp_process(reinterpret_cast<unsigned long>(dev), frame, len);
}

void ArpTable::Dump(Stdlib::Printer& printer)
{
    static const ulong MaxEntries = 16;
    u32 ips[MaxEntries];
    u8 macs[MaxEntries * 6];

    ulong count = rust_arp_snapshot(ips, macs, MaxEntries);
    if (count == 0)
    {
        printer.Printf("arp table empty\n");
        return;
    }

    for (ulong i = 0; i < count; i++)
    {
        Net::IpAddress ip;
        ip.Addr4 = ips[i];
        ip.Print(printer);
        printer.Printf("  ");
        Net::MacAddress(&macs[i * 6]).Print(printer);
        printer.Printf("\n");
    }
}

}
