#include "icmp.h"

#include <net/net_device.h>

extern "C" {

/* What `icmp` reports; crate::icmp::Stats is the same struct. */
struct RustIcmpStats
{
    unsigned long EchoReqRx;
    unsigned long EchoReqTx;
    unsigned long EchoReplyRx;
    unsigned long EchoReplyTx;
    unsigned long EchoReplyTxFail;
    unsigned long RxOther;
    unsigned long RxTooShort;
    unsigned long RxBadCsum;
};

void rust_icmp_process(unsigned long dev, const unsigned char* data, unsigned long len);
int rust_icmp_send_echo(unsigned long dev, unsigned int dst, unsigned short id,
    unsigned short seq);
int rust_icmp_wait_reply(unsigned short id, unsigned short seq, unsigned long timeoutMs,
    unsigned long* rttNs);
void rust_icmp_stats(RustIcmpStats* out);

}

namespace Kernel
{

void Icmp::Process(NetDevice* dev, const u8* frame, ulong len)
{
    if (dev == nullptr || frame == nullptr)
        return;

    rust_icmp_process(reinterpret_cast<unsigned long>(dev), frame, len);
}

bool Icmp::SendEchoRequest(NetDevice* dev, Net::IpAddress dstIp, u16 id, u16 seq)
{
    if (dev == nullptr)
        return false;

    return rust_icmp_send_echo(reinterpret_cast<unsigned long>(dev), dstIp.Addr4, id, seq) == 0;
}

bool Icmp::WaitReply(u16 id, u16 seq, ulong timeoutMs, ulong& rttNs)
{
    return rust_icmp_wait_reply(id, seq, timeoutMs, &rttNs) == 0;
}

void Icmp::Dump(Stdlib::Printer& printer)
{
    RustIcmpStats stats = {};
    rust_icmp_stats(&stats);

    printer.Printf("echo request  rx:%u tx:%u\n", stats.EchoReqRx, stats.EchoReqTx);
    printer.Printf("echo reply    rx:%u tx:%u tx-fail:%u\n",
        stats.EchoReplyRx, stats.EchoReplyTx, stats.EchoReplyTxFail);
    printer.Printf("other         rx:%u short:%u badcsum:%u\n",
        stats.RxOther, stats.RxTooShort, stats.RxBadCsum);
}

}
