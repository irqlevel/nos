#include "net_load.h"

#include <net/net.h>
#include <net/net_frame_pool.h>
#include <kernel/trace.h>
#include <kernel/time.h>
#include <kernel/sched.h>
#include <hal/irqchip.h>
#include <mm/new.h>

namespace Kernel
{

NetLoad::NetLoad()
    : Dev(nullptr)
    , TaskPtr(nullptr)
    , Port(0)
    , Echo(true)
    , Running(false)
    , RxPps(0)
    , TxPps(0)
    , RxBps(0)
{
    ResetCounters();
}

NetLoad::~NetLoad()
{
}

void NetLoad::ResetCounters()
{
    for (ulong i = 0; i < MaxCpus; i++)
    {
        Cpu_[i].RxPackets = 0;
        Cpu_[i].RxBytes = 0;
        Cpu_[i].TxPackets = 0;
        Cpu_[i].TxFailed = 0;
    }

    RxPps = 0;
    TxPps = 0;
    RxBps = 0;
}

void NetLoad::Totals(ulong& rxPackets, ulong& rxBytes, ulong& txPackets, ulong& txFailed)
{
    rxPackets = 0;
    rxBytes = 0;
    txPackets = 0;
    txFailed = 0;

    for (ulong i = 0; i < MaxCpus; i++)
    {
        rxPackets += Cpu_[i].RxPackets;
        rxBytes += Cpu_[i].RxBytes;
        txPackets += Cpu_[i].TxPackets;
        txFailed += Cpu_[i].TxFailed;
    }
}

void NetLoad::OnFrame(NetFrame* frame)
{
    using namespace Net;

    u8* data = frame->Data;
    ulong len = frame->Length;

    if (len < sizeof(EthHdr) + sizeof(IpHdr) + sizeof(UdpHdr))
        return;

    IpHdr* ip = (IpHdr*)(data + sizeof(EthHdr));

    /* Honor IHL, so IP options shift the UDP offset. */
    ulong ipHdrLen = IpHeaderLen(ip);
    if (ipHdrLen == 0 || len < sizeof(EthHdr) + ipHdrLen + sizeof(UdpHdr))
        return;

    UdpHdr* udp = (UdpHdr*)(data + sizeof(EthHdr) + ipHdrLen);

    ulong index = Hal::GetCurrentCpuHwId();
    if (index >= MaxCpus)
        index = 0;

    PerCpu& cpu = Cpu_[index];
    cpu.RxPackets++;
    cpu.RxBytes += len;

    if (!Echo)
        return;

    /* The reply is the frame that arrived, its addresses swapped where they
       lie -- no copy and no allocation, the way netblk answers a write.

       NetDevice::SendUdp must never be called from here. It resolves the
       destination through ArpTable::Resolve, which on a cache miss sends a
       request and then sleeps up to three seconds waiting for the answer.
       This is the receive dispatch path: sleeping in it stops every packet
       the machine would otherwise process, including the ICMP it needs to
       answer a ping and the datagrams carrying the shell. ARP entries expire
       after five minutes, so that miss is not a rare case -- it is one every
       load test long enough to be interesting. */
    EthHdr* eth = (EthHdr*)data;
    Stdlib::MemCpy(eth->DstMac, eth->SrcMac, 6);
    Dev->GetMac().CopyTo(eth->SrcMac);

    auto srcAddr = ip->SrcAddr;
    ip->SrcAddr = ip->DstAddr;
    ip->DstAddr = srcAddr;
    ip->Ttl = 64;
    ip->Checksum = 0;
    ip->Checksum = Htons(IpChecksum(ip, ipHdrLen));

    auto srcPort = udp->SrcPort;
    udp->SrcPort = udp->DstPort;
    udp->DstPort = srcPort;

    /* Zero means "not computed", which IPv4 allows and which is what this
       saves a pass over the payload for. */
    udp->Checksum = 0;

    /* Kept past this callback -- the dispatcher's Put is then not the last
       one -- and sent with the rest of the batch when it ends (BatchEndFn),
       or now, if the batch has filled what is kept. */
    frame->Get();
    Pending[PendingCount++] = frame;
    if (PendingCount == MaxPending)
        FlushReplies();
}

/* The batch's replies to the NIC in one SubmitTxBatch: one TxQueueLock and
   one doorbell for the lot. Each echo used to take both on its own, and
   under a flood the lock's release, just after the doorbell, was where a
   profile found the receive CPU spending most. */
void NetLoad::FlushReplies()
{
    if (PendingCount == 0)
        return;

    ulong queued = Dev->SubmitTxBatch(Pending, PendingCount);

    ulong index = Hal::GetCurrentCpuHwId();
    if (index >= MaxCpus)
        index = 0;

    /* SubmitTxBatch takes every frame: what found no room it releases */
    Cpu_[index].TxPackets += queued;
    Cpu_[index].TxFailed += PendingCount - queued;
    PendingCount = 0;
}

void NetLoad::FrameCallbackFn(void* ctx, NetFrame* frame)
{
    NetLoad* self = static_cast<NetLoad*>(ctx);

    if (!self->Running)
        return;

    self->OnFrame(frame);
}

void NetLoad::BatchEndFn(void* ctx)
{
    static_cast<NetLoad*>(ctx)->FlushReplies();
}

/* A rate from two samples of a counter. `netload reset` zeroes the counters
   under the sampler, so one found below its last sample has started again:
   what it holds is the count since, not a difference to wrap around. */
static ulong CounterDelta(ulong now, ulong last)
{
    return (now >= last) ? (now - last) : now;
}

void NetLoad::Run()
{
    ulong lastRxPackets = 0;
    ulong lastRxBytes = 0;
    ulong lastTxPackets = 0;
    ulong txFailed = 0;

    Totals(lastRxPackets, lastRxBytes, lastTxPackets, txFailed);

    while (!TaskPtr->IsStopping())
    {
        Sleep(SampleMs * Const::NanoSecsInMs);

        ulong rxPackets, rxBytes, txPackets, txFailed;
        Totals(rxPackets, rxBytes, txPackets, txFailed);

        /* One second per sample, so the delta is the rate. */
        RxPps = CounterDelta(rxPackets, lastRxPackets);
        TxPps = CounterDelta(txPackets, lastTxPackets);
        RxBps = CounterDelta(rxBytes, lastRxBytes);

        /* One line a second, over the netconsole, for as long as the load
           runs. The point is not the numbers: it is that the line keeps
           arriving. This machine goes deaf under load -- the shell stops
           answering and so does ping -- and every channel that could say why
           is a network channel. The netconsole only sends, so if these lines
           continue after the machine has stopped receiving, the machine is
           alive and the receive path is what died; if they stop with it, the
           kernel itself is wedged. Nothing else here can tell those apart. */
        Trace(0, "NetLoad: rx %u (+%u), tx %u, failed %u, pool misses %u, "
            "in flight %u, rx polls %u, poll work %u, stalls %u",
            rxPackets, RxPps, txPackets, txFailed,
            NetFramePool::GetInstance().GetAllocMisses(),
            NetFramePool::GetInstance().GetInFlight(),
            NetDeviceTable::GetInstance().GetRxPolls(),
            NetDeviceTable::GetInstance().GetRxPollWork(),
            NetDeviceTable::GetInstance().GetRxStalls());

        lastRxPackets = rxPackets;
        lastRxBytes = rxBytes;
        lastTxPackets = txPackets;
    }
}

void NetLoad::TaskFunc(void* ctx)
{
    NetLoad* self = static_cast<NetLoad*>(ctx);
    self->Run();
}

bool NetLoad::Start(NetDevice* dev, u16 port, bool echo)
{
    if (dev == nullptr || port == 0 || TaskPtr != nullptr)
        return false;

    Dev = dev;
    Port = port;
    Echo = echo;
    ResetCounters();

    TaskPtr = Mm::TAlloc<Task, Tag>("netload");
    if (TaskPtr == nullptr)
    {
        Dev = nullptr;
        Port = 0;
        return false;
    }

    if (!TaskPtr->Start(&NetLoad::TaskFunc, this))
    {
        TaskPtr->Put();
        TaskPtr = nullptr;
        Dev = nullptr;
        Port = 0;
        return false;
    }

    PendingCount = 0;

    /* Listener slots are few, and DHCP, DNS and the shell have taken theirs
       already: a full table is a real outcome and has to be reported, not
       left as a server that is running and never dispatched to -- and so is
       a port someone else has, which a frame listener is refused. */
    int err = Dev->ListenUdpFrames(Port, FrameCallbackFn, this, BatchEndFn);
    if (err != NetDevice::UdpListenOk)
    {
        Trace(0, "NetLoad: cannot listen on UDP port %u: %s", (ulong)Port,
            (err == NetDevice::UdpListenPortTaken) ? "taken" : "no free listener slot");
        TaskPtr->SetStopping();
        TaskPtr->Wait();
        TaskPtr->Put();
        TaskPtr = nullptr;
        Dev = nullptr;
        Port = 0;
        return false;
    }

    Running = true;
    Trace(0, "NetLoad: started on port %u, %s", (ulong)Port, echo ? "echo" : "sink");
    return true;
}

void NetLoad::Stop()
{
    if (TaskPtr == nullptr)
        return;

    /* Before the listener goes: a callback already inside OnFrame finishes,
       and the flag keeps a later one from starting. */
    Running = false;

    /* Returns once no callback of the device's listeners is running -- a
       batch's end included, which hands over what that batch built -- so
       nothing should be left kept here; were anything, it goes out rather
       than leaking. */
    Dev->UnlistenUdpFrames(Port, this);
    FlushReplies();

    TaskPtr->SetStopping();
    TaskPtr->Wait();
    TaskPtr->Put();
    TaskPtr = nullptr;

    Trace(0, "NetLoad: stopped on port %u", (ulong)Port);

    Dev = nullptr;
    Port = 0;
}

void NetLoad::Dump(Stdlib::Printer& printer)
{
    if (TaskPtr == nullptr)
    {
        printer.Printf("netload: not running\n");
        return;
    }

    ulong rxPackets, rxBytes, txPackets, txFailed;
    Totals(rxPackets, rxBytes, txPackets, txFailed);

    printer.Printf("netload: port %u, %s\n", (ulong)Port, Echo ? "echo" : "sink");
    printer.Printf("rx %u packets, %u bytes\n", rxPackets, rxBytes);
    printer.Printf("tx %u packets, %u failed\n", txPackets, txFailed);
    printer.Printf("rate %u rx-pps, %u tx-pps, %u rx-bytes/s\n",
        (ulong)RxPps, (ulong)TxPps, (ulong)RxBps);

    /* Which CPUs the driver's interrupts actually landed on: a load test that
       runs entirely on one core is measuring one core. */
    printer.Printf("per cpu rx:");
    for (ulong i = 0; i < MaxCpus; i++)
    {
        if (Cpu_[i].RxPackets != 0)
            printer.Printf(" %u:%u", i, Cpu_[i].RxPackets);
    }
    printer.Printf("\n");
}

}
