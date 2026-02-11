#pragma once

#include <include/types.h>
#include <net/net_device.h>
#include <kernel/spin_lock.h>

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

    bool Lookup(u32 ip, u8 mac[6]);
    void Insert(u32 ip, const u8 mac[6]);

    /* Resolve IP to MAC. Sends ARP request and polls for reply. */
    bool Resolve(NetDevice* dev, u32 ip, u8 mac[6]);

    /* Process incoming ARP frame (request or reply). */
    void Process(NetDevice* dev, const u8* frame, ulong len);

private:
    ArpTable();
    ~ArpTable();
    ArpTable(const ArpTable& other) = delete;
    ArpTable(ArpTable&& other) = delete;
    ArpTable& operator=(const ArpTable& other) = delete;
    ArpTable& operator=(ArpTable&& other) = delete;

    void SendRequest(NetDevice* dev, u32 ip);
    void SendReply(NetDevice* dev, const u8* reqFrame);

    struct ArpEntry
    {
        u32 Ip;
        u8 Mac[6];
        bool Valid;
    };

    static const ulong CacheSize = 16;
    ArpEntry Cache[CacheSize];
    SpinLock Lock;
};

}
