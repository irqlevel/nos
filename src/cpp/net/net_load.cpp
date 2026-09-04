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

void NetLoad::OnFrame(const u8* frame, ulong len)
{
    using namespace Net;

    if (len < sizeof(EthHdr) + sizeof(IpHdr) + sizeof(UdpHdr))
        return;

    const IpHdr* ip = (const IpHdr*)(frame + sizeof(EthHdr));

    /* Honor IHL, so IP options shift the UDP offset. */
    ulong ipHdrLen = IpHeaderLen(ip);
    if (ipHdrLen == 0)
        return;

    ulong hdrLen = sizeof(EthHdr) + ipHdrLen + sizeof(UdpHdr);
    if (len < hdrLen)
        return;

    const UdpHdr* udp = (const UdpHdr*)(frame + sizeof(EthHdr) + ipHdrLen);

    const u8* payload = frame + hdrLen;
    ulong payloadLen = len - hdrLen;

    ulong index = Hal::GetCurrentCpuHwId();
    if (index >= MaxCpus)
        index = 0;

    PerCpu& cpu = Cpu_[index];
    cpu.RxPackets++;
    cpu.RxBytes += len;

    if (!Echo)
        return;

    (void)payload;
    (void)payloadLen;

    if (len > MaxFrameLen)
    {
        cpu.TxFailed++;
        return;
    }

    /* The reply is the frame that arrived, with its addresses swapped -- the
       way Icmp answers a ping, and for the same reason.

       NetDevice::SendUdp must never be called from here. It resolves the
       destination through ArpTable::Resolve, which on a cache miss sends a
       request and then sleeps up to three seconds waiting for the answer.
       This is the receive dispatch path: sleeping in it stops every packet
       the machine would otherwise process, including the ICMP it needs to
       answer a ping and the datagrams carrying the shell. ARP entries expire
       after five minutes, so that miss is not a rare case -- it is one every
       load test long enough to be interesting.

       Swapping also costs less: SendUdp clears and rebuilds a 1514-byte
       frame per packet, where everything needed is already here. */
    u8 reply[MaxFrameLen];
    Stdlib::MemCpy(reply, frame, len);

    EthHdr* rEth = (EthHdr*)reply;
    Stdlib::MemCpy(rEth->DstMac, ((const EthHdr*)frame)->SrcMac, 6);
    Dev->GetMac().CopyTo(rEth->SrcMac);

    IpHdr* rIp = (IpHdr*)(reply + sizeof(EthHdr));
    rIp->SrcAddr = ip->DstAddr;
    rIp->DstAddr = ip->SrcAddr;
    rIp->Ttl = 64;
    rIp->Checksum = 0;
    rIp->Checksum = Htons(IpChecksum(rIp, ipHdrLen));

    UdpHdr* rUdp = (UdpHdr*)(reply + sizeof(EthHdr) + ipHdrLen);
    rUdp->SrcPort = udp->DstPort;
    rUdp->DstPort = udp->SrcPort;

    /* Zero means "not computed", which IPv4 allows and which is what this
       saves a pass over the payload for. */
    rUdp->Checksum = 0;

    if (Dev->SendRaw(reply, len))
        cpu.TxPackets++;
    else
        cpu.TxFailed++;
}

void NetLoad::RxCallbackFn(const u8* frame, ulong len, void* ctx)
{
    NetLoad* self = static_cast<NetLoad*>(ctx);

    if (!self->Running)
        return;

    self->OnFrame(frame, len);
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
        RxPps = rxPackets - lastRxPackets;
        TxPps = txPackets - lastTxPackets;
        RxBps = rxBytes - lastRxBytes;

        /* One line a second, over the netconsole, for as long as the load
           runs. The point is not the numbers: it is that the line keeps
           arriving. This machine goes deaf under load -- the shell stops
           answering and so does ping -- and every channel that could say why
           is a network channel. The netconsole only sends, so if these lines
           continue after the machine has stopped receiving, the machine is
           alive and the receive path is what died; if they stop with it, the
           kernel itself is wedged. Nothing else here can tell those apart. */
        Trace(0, "NetLoad: rx %u (+%u), tx %u, failed %u, pool misses %u, in flight %u",
            rxPackets, RxPps, txPackets, txFailed,
            NetFramePool::GetInstance().GetAllocMisses(),
            NetFramePool::GetInstance().GetInFlight());

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

    /* Listener slots are few, and DHCP, DNS and the shell have taken theirs
       already: a full table is a real outcome and has to be reported, not
       left as a server that is running and never dispatched to. */
    if (!Dev->RegisterUdpListener(Port, RxCallbackFn, this))
    {
        Trace(0, "NetLoad: no free UDP listener slot for port %u", (ulong)Port);
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

    Dev->UnregisterUdpListener(Port);

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
