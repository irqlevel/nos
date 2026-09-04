#include "net_load.h"

#include <net/net.h>
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

    IpAddress senderIp = IpAddress::FromNetwork(ip->SrcAddr);
    u16 senderPort = Ntohs(udp->SrcPort);

    /* Straight back out from the receive path, the way Icmp answers a ping.
       The payload pointer is into the receive frame and SendUdp copies it,
       so nothing here outlives the call. */
    if (Dev->SendUdp(senderIp, senderPort, Dev->GetIp(), Port, payload, payloadLen))
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
