#include "net_load.h"

#include <net/net_device.h>
#include <kernel/cpu.h>

extern "C" {

/* What `netload` reports; crate::net_load::Stats is the same struct. */
struct RustNetLoadStats
{
    unsigned int Running;
    unsigned short Port;
    unsigned short Echo;
    unsigned long RxPackets;
    unsigned long RxBytes;
    unsigned long TxPackets;
    unsigned long TxFailed;
    unsigned long RxPps;
    unsigned long TxPps;
    unsigned long RxBps;
};

int rust_netload_start(unsigned long dev, unsigned short port, int echo);
void rust_netload_stop();
int rust_netload_running();
void rust_netload_reset();
void rust_netload_stats(RustNetLoadStats* out);
unsigned long rust_netload_cpu_rx(unsigned long index);

}

namespace Kernel
{

bool NetLoad::Start(NetDevice* dev, u16 port, bool echo)
{
    if (dev == nullptr)
        return false;

    return rust_netload_start(reinterpret_cast<unsigned long>(dev), port,
        echo ? 1 : 0) == 0;
}

void NetLoad::Stop()
{
    rust_netload_stop();
}

bool NetLoad::IsRunning()
{
    return rust_netload_running() != 0;
}

void NetLoad::ResetCounters()
{
    rust_netload_reset();
}

void NetLoad::Dump(Stdlib::Printer& printer)
{
    RustNetLoadStats stats = {};
    rust_netload_stats(&stats);

    if (stats.Running == 0)
    {
        printer.Printf("netload: not running\n");
        return;
    }

    printer.Printf("netload: port %u, %s\n", (ulong)stats.Port,
        stats.Echo ? "echo" : "sink");
    printer.Printf("rx %u packets, %u bytes\n", stats.RxPackets, stats.RxBytes);
    printer.Printf("tx %u packets, %u failed\n", stats.TxPackets, stats.TxFailed);
    printer.Printf("rate %u rx-pps, %u tx-pps, %u rx-bytes/s\n",
        stats.RxPps, stats.TxPps, stats.RxBps);

    /* Which CPUs the driver's interrupts actually landed on: a load test that
       runs entirely on one core is measuring one core. */
    printer.Printf("per cpu rx:");
    for (ulong i = 0; i < MaxCpus; i++)
    {
        ulong rx = rust_netload_cpu_rx(i);
        if (rx != 0)
            printer.Printf(" %u:%u", i, rx);
    }
    printer.Printf("\n");
}

}
