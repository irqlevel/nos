#pragma once

#include <include/types.h>
#include <net/net.h>
#include <lib/printer.h>

namespace Kernel
{

struct NetDevice;

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

/* ICMP itself is Rust (src/rust/net/src/icmp.rs): the echo requests and
   replies, and the unreachables that abort a TCP connection. What is left
   here is the way in. */
class Icmp
{
public:
    static Icmp& GetInstance()
    {
        static Icmp instance;
        return instance;
    }

    /* An ICMP packet off the wire, from the receive path. */
    void Process(NetDevice* dev, const u8* frame, ulong len);

    bool SendEchoRequest(NetDevice* dev, Net::IpAddress dstIp, u16 id, u16 seq);

    /* The round trip of the reply to (id, seq) into rttNs; false once the
       timeout passes with none. */
    bool WaitReply(u16 id, u16 seq, ulong timeoutMs, ulong& rttNs);

    void Dump(Stdlib::Printer& printer);

    static const u8 TypeEchoReply   = 0;
    static const u8 TypeDestUnreach = 3;
    static const u8 TypeEchoRequest = 8;

    /* Destination Unreachable codes that are hard errors for TCP
       (RFC 1122 4.2.3.9) */
    static const u8 CodeProtoUnreach = 2;
    static const u8 CodePortUnreach  = 3;

private:
    Icmp() {}
    ~Icmp() {}
    Icmp(const Icmp& other) = delete;
    Icmp(Icmp&& other) = delete;
    Icmp& operator=(const Icmp& other) = delete;
    Icmp& operator=(Icmp&& other) = delete;
};

}
