#pragma once

#include <include/types.h>
#include <lib/printer.h>
#include <lib/list_entry.h>
#include <kernel/spin_lock.h>
#include <kernel/raw_spin_lock.h>
#include <net/net.h>
#include <net/net_frame.h>

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
    virtual u64 GetTxPackets() = 0;
    virtual u64 GetRxPackets() = 0;
    virtual u64 GetRxDropped() = 0;
    virtual void GetStats(NetStats& stats) { (void)stats; }

    /* TX: enqueue frame to TxQueue, call FlushTx() */
    bool SubmitTx(NetFrame* frame);

    /* TX: convenience wrapper -- alloc frame, copy data, SubmitTx */
    bool SendRaw(const void* buf, ulong len);

    /* RX: enqueue frame to RxQueue; returns false if full (caller must Put) */
    bool EnqueueRx(NetFrame* frame);

    Net::MacAddress GetMac();
    void SetMac(const Net::MacAddress& mac);
    Net::IpAddress GetIp();
    void SetIp(Net::IpAddress ip);
    Net::IpAddress GetSubnetMask();
    void SetSubnetMask(Net::IpAddress mask);
    Net::IpAddress GetGateway();
    void SetGateway(Net::IpAddress gw);

    /* Return the IP to ARP for: gateway if dstIp is off-subnet, else dstIp */
    Net::IpAddress RouteIp(Net::IpAddress dstIp);

    typedef void (*RxCallback)(const u8* frame, ulong len, void* ctx);

    bool RegisterUdpListener(u16 port, RxCallback cb, void* ctx);
    void UnregisterUdpListener(u16 port);

    /* Higher-level UDP send: builds the Ethernet/IP/UDP headers, resolves the
       destination MAC through ARP and hands the frame to SendRaw. Generic
       protocol code, so it lives here rather than in any one driver -- a
       driver that returned false from this is a driver whose UDP replies
       (the UDP shell, DNS) silently never leave the box. */
    virtual bool SendUdp(Net::IpAddress dstIp, u16 dstPort, Net::IpAddress srcIp, u16 srcPort,
                         const void* data, ulong len);

    static const ulong MaxUdpListeners = 4;

    struct UdpListener
    {
        u16 Port;
        RxCallback Cb;
        void* Ctx;
    };

    /* Driver must implement: drain TxQueue to hardware (called under TxQueueLock) */
    virtual void FlushTx() = 0;

    /* Driver must implement: process frames from RxQueue (called from softirq) */
    virtual void ProcessRx() = 0;

    /* Driver hook: reap completed RX buffers from hardware into RxQueue
       (called from the net RX softirq before ProcessRx) */
    virtual void ReapRx() {}

    /* Driver hook: retry pending TX (called from the net TX softirq).
       Default: flush TxQueue under TxQueueLock. */
    virtual void DrainTx();

    /* Drain the SW RxQueue and dispatch frames to the protocol stack. Drivers
       whose reap runs as ReapRx() can use this as their ProcessRx(). */
    void DrainRxQueueAndDispatch();

    /* A driver must never release a transmitted frame from inside FlushTx.
       FlushTx runs under TxQueueLock with interrupts off, and NetFrame::Put
       reaches Mm::Free, which shoots down the TLB on every other CPU and
       waits for each one to acknowledge -- and a CPU spinning on TxQueueLock
       has interrupts off, so it never can. The two then wait for each other
       forever and the machine stops dead, with no fault to panic on and the
       netconsole drain blocked on the same lock, so it cannot even say so.

       Hand the frame here instead. SubmitTx and DrainTx empty the queue once
       the lock is down. */
    void TxDone(NetFrame* frame);
    void ReleaseTxDone();

protected:
    static const ulong TxQueueCapacity = 256;
    static const ulong RxQueueCapacity = 256;

    Stdlib::ListEntry TxQueue;
    ulong TxCount;
    RawSpinLock TxQueueLock;

    /* Transmitted frames waiting to be released outside the lock. */
    Stdlib::ListEntry TxDoneQueue;

    Stdlib::ListEntry RxQueue;
    ulong RxCount;
    RawSpinLock RxQueueLock;

    UdpListener UdpListeners[MaxUdpListeners];
    ulong UdpListenerCount;
    SpinLock UdpListenerLock;
    Net::MacAddress Mac;
    Net::IpAddress Ip;
    Net::IpAddress Mask;
    Net::IpAddress Gw;
};

/* Not internally synchronized: Register() is expected to run only at boot
   (driver probe) before any concurrent Find()/Dump() readers exist. Devices are
   never unregistered, so steady-state reads need no lock. */
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

    /* Softirq-driven RX/TX processing across all registered devices */
    void ProcessAllRx();
    void ProcessAllTx();

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
