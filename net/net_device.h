#pragma once

#include <include/types.h>
#include <lib/printer.h>

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
    virtual ~NetDevice() {}
    virtual const char* GetName() = 0;
    virtual void GetMac(u8 mac[6]) = 0;
    virtual bool SendRaw(const void* buf, ulong len) = 0;
    virtual u64 GetTxPackets() = 0;
    virtual u64 GetRxPackets() = 0;
    virtual u64 GetRxDropped() = 0;
    virtual void GetStats(NetStats& stats) { (void)stats; }

    virtual u32 GetIp() { return 0; }
    virtual void SetIp(u32 ip) { (void)ip; }

    typedef void (*RxCallback)(const u8* frame, ulong len, void* ctx);
    virtual void SetRxCallback(RxCallback cb, void* ctx) { (void)cb; (void)ctx; }
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
