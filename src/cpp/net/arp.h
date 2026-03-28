#pragma once

#include <include/types.h>
#include <net/net_device.h>
#include <kernel/spin_lock.h>
#include <lib/printer.h>

namespace Kernel
{

class ArpTable
{
public:
    static ArpTable& GetInstance()
    {
        static ArpTable instance;
        return instance;
    }

    bool Lookup(Net::IpAddress ip, Net::MacAddress& mac);
    void Insert(Net::IpAddress ip, const Net::MacAddress& mac);

    /* Resolve IP to MAC. Sends ARP request and polls for reply. */
    bool Resolve(NetDevice* dev, Net::IpAddress ip, Net::MacAddress& mac);

    /* Process incoming ARP frame (request or reply). */
    void Process(NetDevice* dev, const u8* frame, ulong len);

    /* Dump ARP cache contents. */
    void Dump(Stdlib::Printer& printer);

private:
    ArpTable();
    ~ArpTable();
    ArpTable(const ArpTable& other) = delete;
    ArpTable(ArpTable&& other) = delete;
    ArpTable& operator=(const ArpTable& other) = delete;
    ArpTable& operator=(ArpTable&& other) = delete;

    void SendRequest(NetDevice* dev, Net::IpAddress ip);
    void SendReply(NetDevice* dev, const u8* reqFrame);

    struct ArpEntry
    {
        Net::IpAddress Ip;
        Net::MacAddress Mac;
        bool Valid;
    };

    static const ulong CacheSize = 16;
    ArpEntry Cache[CacheSize];
    SpinLock Lock;
};

}
