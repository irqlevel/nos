#pragma once

#include <include/types.h>
#include <net/net.h>
#include <lib/printer.h>

namespace Kernel
{

/* The network devices are Rust (src/rust/net/src/device.rs): the queues
   between a driver and the stack, the UDP listeners, the receive dispatch
   and the per-CPU counters. What is left here is the way in.

   A device is opaque on this side: the table lives in Rust and a NetDevice*
   is a handle into it, stable for the life of the kernel -- nothing takes a
   device back. */
struct NetDevice;

class NetDeviceTable
{
public:
    static NetDeviceTable& GetInstance()
    {
        static NetDeviceTable instance;
        return instance;
    }

    NetDevice* Find(const char* name);
    ulong GetCount();

    /* Look at the receive path without waiting to be asked: a driver whose
       only source of liveness is its own interrupt has no recovery from a
       lost one, and this makes that cost a tick rather than the uptime. */
    void PollRx();

    /* Polls issued; polls that found frames waiting; and polls that found
       frames waiting with no interrupt-driven pass since the previous poll.
       Only the third is evidence of a lost wakeup. */
    ulong GetRxPolls();
    ulong GetRxPollWork();
    ulong GetRxStalls();

    void Dump(Stdlib::Printer& printer);

private:
    NetDeviceTable() {}
    ~NetDeviceTable() {}
    NetDeviceTable(const NetDeviceTable& other) = delete;
    NetDeviceTable& operator=(const NetDeviceTable& other) = delete;
};

/* What the shell asks of one device */
const char* NetDeviceName(NetDevice* dev, char* buf, ulong bufSize);
Net::IpAddress NetDeviceIp(NetDevice* dev);

/* A UDP datagram out of the device: the headers built, the destination
   resolved through ARP. Task context -- the resolution may wait. */
bool NetDeviceSendUdp(NetDevice* dev, Net::IpAddress dstIp, u16 dstPort,
                      Net::IpAddress srcIp, u16 srcPort, const void* data, ulong len);

}
