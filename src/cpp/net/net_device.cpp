#include "net_device.h"

#include <lib/stdlib.h>

extern "C" {

/* What `net` prints per device; crate::device::Stats is the same struct. */
struct RustNetStats
{
    unsigned long TxTotal;
    unsigned long RxTotal;
    unsigned long RxDrop;
    unsigned long RxIcmp;
    unsigned long RxUdp;
    unsigned long RxTcp;
    unsigned long RxArp;
    unsigned long RxOther;
    unsigned long TxIcmp;
    unsigned long TxUdp;
    unsigned long TxTcp;
    unsigned long TxArp;
    unsigned long TxOther;
};

unsigned long kernel_net_find(const unsigned char* name, unsigned long nameLen);
unsigned int kernel_net_ip(unsigned long dev);
void kernel_net_mac(unsigned long dev, unsigned char* out);
void rust_net_poll_rx();
void kernel_net_rx_poll_stats(unsigned long* polls, unsigned long* work,
    unsigned long* stalls);
unsigned long rust_net_device_count();
unsigned long rust_net_device_at(unsigned long index);
unsigned long rust_net_device_name(unsigned long dev, char* out, unsigned long cap);
void rust_net_device_stats(unsigned long dev, RustNetStats* out);
int rust_net_send_udp(unsigned long dev, unsigned int dstIp, unsigned short dstPort,
    unsigned int srcIp, unsigned short srcPort, const unsigned char* data,
    unsigned long len);

}

namespace Kernel
{

NetDevice* NetDeviceTable::Find(const char* name)
{
    if (name == nullptr)
        return nullptr;

    return (NetDevice*)kernel_net_find((const unsigned char*)name,
        Stdlib::StrLen(name));
}

ulong NetDeviceTable::GetCount()
{
    return rust_net_device_count();
}

void NetDeviceTable::PollRx()
{
    rust_net_poll_rx();
}

ulong NetDeviceTable::GetRxPolls()
{
    ulong polls = 0;
    kernel_net_rx_poll_stats(&polls, nullptr, nullptr);
    return polls;
}

ulong NetDeviceTable::GetRxPollWork()
{
    ulong work = 0;
    kernel_net_rx_poll_stats(nullptr, &work, nullptr);
    return work;
}

ulong NetDeviceTable::GetRxStalls()
{
    ulong stalls = 0;
    kernel_net_rx_poll_stats(nullptr, nullptr, &stalls);
    return stalls;
}

const char* NetDeviceName(NetDevice* dev, char* buf, ulong bufSize)
{
    if (dev == nullptr || buf == nullptr || bufSize == 0)
        return "";

    buf[0] = '\0';
    rust_net_device_name(reinterpret_cast<unsigned long>(dev), buf, bufSize);
    return buf;
}

Net::IpAddress NetDeviceIp(NetDevice* dev)
{
    Net::IpAddress ip;
    ip.Addr4 = (dev != nullptr)
        ? kernel_net_ip(reinterpret_cast<unsigned long>(dev)) : 0;
    return ip;
}

bool NetDeviceSendUdp(NetDevice* dev, Net::IpAddress dstIp, u16 dstPort,
                      Net::IpAddress srcIp, u16 srcPort, const void* data, ulong len)
{
    if (dev == nullptr)
        return false;

    return rust_net_send_udp(reinterpret_cast<unsigned long>(dev), dstIp.Addr4,
        dstPort, srcIp.Addr4, srcPort, (const unsigned char*)data, len) == 0;
}

void NetDeviceTable::Dump(Stdlib::Printer& printer)
{
    ulong count = rust_net_device_count();
    if (count == 0)
    {
        printer.Printf("no network devices\n");
        return;
    }

    for (ulong i = 0; i < count; i++)
    {
        ulong dev = rust_net_device_at(i);
        if (dev == 0)
            continue;

        char name[32];
        name[0] = '\0';
        rust_net_device_name(dev, name, sizeof(name));

        unsigned char macBytes[6] = {};
        kernel_net_mac(dev, macBytes);
        Net::MacAddress mac(macBytes);

        Net::IpAddress ip;
        ip.Addr4 = kernel_net_ip(dev);

        RustNetStats st = {};
        rust_net_device_stats(dev, &st);

        printer.Printf("%s  ", name);
        mac.Print(printer);
        printer.Printf("  ip:");
        ip.Print(printer);
        printer.Printf("  tx:%u rx:%u drop:%u\n", st.TxTotal, st.RxTotal, st.RxDrop);
        printer.Printf("  rx  icmp:%u udp:%u tcp:%u arp:%u other:%u\n",
            st.RxIcmp, st.RxUdp, st.RxTcp, st.RxArp, st.RxOther);
        printer.Printf("  tx  icmp:%u udp:%u tcp:%u arp:%u other:%u\n",
            st.TxIcmp, st.TxUdp, st.TxTcp, st.TxArp, st.TxOther);
    }
}

}
