#pragma once

#include <include/types.h>
#include <lib/printer.h>

namespace Kernel
{

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
