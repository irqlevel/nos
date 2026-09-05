#pragma once

#include <include/types.h>
#include <lib/printer.h>
#include <lib/list_entry.h>
#include <kernel/spin_lock.h>
#include <kernel/raw_spin_lock.h>
#include <net/net.h>
#include <net/net_frame.h>
#include <kernel/cpu.h>

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

    /* Frames reaped from the hardware and waiting to be dispatched. Read
       without the lock: it is a hint for the receive poll, not an invariant. */
    ulong GetRxPending() { return RxCount; }

    /* RX: enqueue frame to RxQueue; returns false if full (caller must Put) */
    bool EnqueueRx(NetFrame* frame);

    /* Enqueue a run of frames under one lock acquisition. Returns how many
       were taken -- the queue may fill part way -- and the caller releases
       the remainder. */
    ulong EnqueueRxBatch(NetFrame** frames, ulong count);

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

    /* Listener callbacks running right now. The dispatcher takes a listener
       out of the table under UdpListenerLock and then calls it with the lock
       released -- so a callback runs with interrupts on, may take locks of
       its own, and no longer stalls the NIC's interrupt for its duration.
       UnregisterUdpListener waits for this to drain before returning, which
       is what makes freeing the callback's context after it safe. */
    Atomic UdpListenerInFlight;

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

    /* Counted by DrainRxQueueAndDispatch, which is the one place every
       driver's frames pass through -- they used to be per-driver, so the
       `net` command reported zeros for any device whose driver had not
       written its own copy of the dispatch loop, which is how the Rust NIC
       bridge came to show tx:0 rx:0 on a machine forwarding thousands of
       packets a second.

       Per CPU and plain, not shared and atomic. This is a datapath, and a
       counter every arriving packet increments on one cache line is the
       thing this kernel has spent a day removing from other datapaths --
       the frame pool and the load target both count this way for the same
       reason. The CPU is read once per batch, not once per frame. */
    struct RxProtoCounters
    {
        ulong Icmp;
        ulong Udp;
        ulong Tcp;
        ulong Arp;
        ulong Other;
        ulong Drop;

        /* Padded to a cache line rather than aligned to one: alignas here
           would over-align NetDevice itself, and deleting an over-aligned
           object calls operator delete(void*, align_val_t), which this
           freestanding runtime does not provide. Sizing to 64 keeps two CPUs
           off the same line everywhere except the array's own ends. */
        ulong Pad[2];
    };

    static_assert(sizeof(RxProtoCounters) == 64, "one counter set per line");

    RxProtoCounters RxProto[MaxCpus];

    /* The same on the way out. These were six shared atomics per frame
       taken inside FlushTx, which runs under TxQueueLock with interrupts
       off -- the narrowest section on the transmit path. */
    struct TxProtoCounters
    {
        ulong Icmp;
        ulong Udp;
        ulong Tcp;
        ulong Arp;
        ulong Other;
        ulong Total;
        ulong Pad[2];
    };

    static_assert(sizeof(TxProtoCounters) == 64, "one counter set per line");

    TxProtoCounters TxProto[MaxCpus];

    /* Classify one outgoing frame. Called from SubmitTx, which is where
       every driver's frames leave, so the Rust NIC bridge is counted too --
       it reported zeros for the transmit breakdown, having no classifier of
       its own. */
    void CountTxFrame(NetFrame* frame);

    /* Summed across CPUs; for GetStats, never for the datapath. */
    void GetRxProtoTotals(NetStats& stats);
    void GetTxProtoTotals(NetStats& stats);

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

    /* Look at the receive path without waiting to be asked.

       A driver whose only source of liveness is its own interrupt has no
       recovery from a lost one: r8125 reaps only from the receive softirq,
       which is raised only from its ISR, so a wakeup lost while the ring is
       full means nothing ever looks at that ring again -- which is exactly
       how the bare metal machine goes permanently deaf while the rest of the
       kernel runs on. This is the fallback: a lost wakeup then costs a tick
       instead of the rest of the uptime.

       Raised only when the softirq is not already pending, so that a pass it
       causes can be told from one an interrupt caused -- which is what makes
       RxStalls evidence rather than a guess. */
    void PollRx();

    /* Polls issued; polls that found frames waiting; and polls that found
       frames waiting with no interrupt-driven pass since the previous poll.

       Only the third is evidence of a lost wakeup. The second is mostly a
       race that means nothing is wrong: under load the poll often gets there
       before an interrupt that is already on its way, which is why it counts
       in the hundreds on QEMU, where nothing is lost at all. A wakeup that
       is genuinely gone shows as poll after poll finding work with no
       interrupt in between. */
    ulong GetRxPolls();
    ulong GetRxPollWork();
    ulong GetRxStalls();

    static const ulong MaxDevices = 16;

private:
    Atomic PollPending;
    Atomic RxPolls;
    Atomic RxPollWork;
    Atomic RxStalls;

    /* Whether the previous receive pass was one this poll caused. */
    bool LastPassWasPoll;
public:

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
