#pragma once

#include <include/types.h>
#include <lib/printer.h>
#include <kernel/spin_lock.h>
#include <net/net.h>

namespace Kernel
{

struct NetStats
{
    u64 TxTotal;
    u64 RxTotal;
    u64 RxDrop;
    u64 RxIcmp;
    u64 RxUdp;
    u64 RxTcp;
    u64 RxArp;
    u64 RxOther;
    u64 TxIcmp;
    u64 TxUdp;
    u64 TxTcp;
    u64 TxArp;
    u64 TxOther;
};

class NetDevice
{
public:
    NetDevice();
    virtual ~NetDevice() {}
    virtual const char* GetName() = 0;
    virtual bool SendRaw(const void* buf, ulong len) = 0;
    virtual u64 GetTxPackets() = 0;
    virtual u64 GetRxPackets() = 0;
    virtual u64 GetRxDropped() = 0;
    virtual void GetStats(NetStats& stats) { (void)stats; }

    Net::MacAddress GetMac();
    void SetMac(const Net::MacAddress& mac);
    Net::IpAddress GetIp();
    void SetIp(Net::IpAddress ip);

    typedef void (*RxCallback)(const u8* frame, ulong len, void* ctx);

    bool RegisterUdpListener(u16 port, RxCallback cb, void* ctx);
    void UnregisterUdpListener(u16 port);

    /* Higher-level UDP send (implemented by drivers that support it) */
    virtual bool SendUdp(Net::IpAddress dstIp, u16 dstPort, Net::IpAddress srcIp, u16 srcPort,
                         const void* data, ulong len)
    {
        (void)dstIp; (void)dstPort; (void)srcIp; (void)srcPort; (void)data; (void)len;
        return false;
    }

    static const ulong MaxUdpListeners = 4;

    struct UdpListener
    {
        u16 Port;
        RxCallback Cb;
        void* Ctx;
    };

protected:
    UdpListener UdpListeners[MaxUdpListeners];
    ulong UdpListenerCount;
    SpinLock UdpListenerLock;
    Net::MacAddress Mac;
    Net::IpAddress Ip;
};

class NetDeviceTable
{
public:
    static NetDeviceTable& GetInstance()
    {
        static NetDeviceTable instance;
        return instance;
    }

    bool Register(NetDevice* dev);

    NetDevice* Find(const char* name);

    void Dump(Stdlib::Printer& printer);

    ulong GetCount();

    static const ulong MaxDevices = 16;

private:
    NetDeviceTable();
    ~NetDeviceTable();
    NetDeviceTable(const NetDeviceTable& other) = delete;
    NetDeviceTable(NetDeviceTable&& other) = delete;
    NetDeviceTable& operator=(const NetDeviceTable& other) = delete;
    NetDeviceTable& operator=(NetDeviceTable&& other) = delete;

    NetDevice* Devices[MaxDevices];
    ulong Count;
};

}
