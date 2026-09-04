#pragma once

#include <include/types.h>
#include <lib/printer.h>
#include <lib/stdlib.h>
#include <net/net_device.h>
#include <kernel/task.h>
#include <kernel/cpu.h>

namespace Kernel
{

/* A UDP server that exists to be hammered from outside, so that `profile`
   has something to look at other than an idle machine.
 
   An idle 20-CPU box spends about 0.06% of a core on anything at all, and at
   that level the profile is dominated by the tick's own bookkeeping. The
   paths worth seeing -- the frame pool, the driver rings, softirq dispatch,
   the TLB shootdown behind every free -- only appear when packets are
   actually moving.
 
   Echo mode answers every datagram, which exercises receive and transmit
   together; sink mode drops them, which isolates the receive half. The reply
   is built and sent from the receive callback itself, in softirq context,
   the same way Icmp answers a ping: a load generator that woke a task per
   packet would be measuring the wakeup. */
class NetLoad final
{
public:
    static NetLoad& GetInstance()
    {
        static NetLoad Instance;
        return Instance;
    }

    bool Start(NetDevice* dev, u16 port, bool echo);
    void Stop();

    bool IsRunning() const { return Running; }

    void ResetCounters();
    void Dump(Stdlib::Printer& printer);

    static const u16 DefaultPort = 9999;

    /* Ethernet MTU plus headers: the reply is the request with its addresses
       swapped, so it is never larger than what arrived. */
    static const ulong MaxFrameLen = 1514;

private:
    NetLoad();
    ~NetLoad();
    NetLoad(const NetLoad& other) = delete;
    NetLoad& operator=(const NetLoad& other) = delete;

    static void RxCallbackFn(const u8* frame, ulong len, void* ctx);
    void OnFrame(const u8* frame, ulong len);

    static void TaskFunc(void* ctx);
    void Run();

    /* Counters are per CPU and plain, not atomic. This is the datapath the
       profile is about: one shared cache line incremented by twenty cores
       would be the loudest line in the report, and the report would be about
       the instrument. A count lost to a migration between reading the CPU id
       and adding to its slot costs a statistic nothing worth an atomic. */
    struct __attribute__((aligned(64))) PerCpu
    {
        ulong RxPackets;
        ulong RxBytes;
        ulong TxPackets;
        ulong TxFailed;
    };

    PerCpu Cpu_[MaxCpus];

    void Totals(ulong& rxPackets, ulong& rxBytes, ulong& txPackets, ulong& txFailed);

    NetDevice* Dev;
    Task* TaskPtr;
    u16 Port;
    bool Echo;
    volatile bool Running;

    /* Sampled once a second by the task, so the shell can report a rate
       rather than a total nobody can divide in their head. */
    volatile ulong RxPps;
    volatile ulong TxPps;
    volatile ulong RxBps;

    static const ulong SampleMs = 1000;
    static const ulong Tag = 'NetL';
};

}
