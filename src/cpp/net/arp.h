#pragma once

#include <include/types.h>
#include <net/net.h>
#include <lib/printer.h>

namespace Kernel
{

class NetDevice;

/* ARP itself is Rust (src/rust/net/src/arp.rs): the cache, its expiry, the
   requests and the replies. What is left here is the way in. */
class ArpTable
{
public:
    static ArpTable& GetInstance()
    {
        static ArpTable instance;
        return instance;
    }

    /* The cache alone, without asking: false when there is no unexpired
       entry. Any context. */
    bool Lookup(Net::IpAddress ip, Net::MacAddress& mac);

    /* The cache, or an ARP request and a wait for the answer. Task context:
       it sleeps a second at a time, up to three times. */
    bool Resolve(NetDevice* dev, Net::IpAddress ip, Net::MacAddress& mac);

    /* An ARP frame off the wire: a request for us is answered, and either
       kind teaches the cache. */
    void Process(NetDevice* dev, const u8* frame, ulong len);

    void Dump(Stdlib::Printer& printer);

private:
    ArpTable() {}
    ~ArpTable() {}
    ArpTable(const ArpTable& other) = delete;
    ArpTable(ArpTable&& other) = delete;
    ArpTable& operator=(const ArpTable& other) = delete;
    ArpTable& operator=(ArpTable&& other) = delete;
};

}
