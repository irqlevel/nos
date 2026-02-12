#pragma once

#include <include/types.h>
#include <net/net_device.h>
#include <kernel/spin_lock.h>
#include <lib/stdlib.h>

namespace Kernel
{

namespace Net
{

struct IcmpHdr
{
    u8 Type;
    u8 Code;
    u16 Checksum;
    u16 Id;
    u16 Seq;
} __attribute__((packed));

static_assert(sizeof(IcmpHdr) == 8, "Invalid size");

} /* namespace Net */

class Icmp
{
public:
    static Icmp& GetInstance()
    {
        static Icmp instance;
        return instance;
    }

    /* Process incoming ICMP packet (called from DrainRx). */
    void Process(NetDevice* dev, const u8* frame, ulong len);

    /* Send an ICMP echo request to dstIp with given id and seq. */
    bool SendEchoRequest(NetDevice* dev, u32 dstIp, u16 id, u16 seq);

    /* Wait for a matching echo reply. Returns true and sets rttNs on success. */
    bool WaitReply(u16 id, u16 seq, ulong timeoutMs, ulong& rttNs);

    static const u8 TypeEchoReply   = 0;
    static const u8 TypeEchoRequest = 8;

private:
    Icmp();
    ~Icmp();
    Icmp(const Icmp& other) = delete;
    Icmp(Icmp&& other) = delete;
    Icmp& operator=(const Icmp& other) = delete;
    Icmp& operator=(Icmp&& other) = delete;

    struct ReplySlot
    {
        bool Valid;
        u16 Id;
        u16 Seq;
        Stdlib::Time Timestamp;
    };

    ReplySlot Reply;
    Stdlib::Time SendTime;
    SpinLock Lock;
};

}
