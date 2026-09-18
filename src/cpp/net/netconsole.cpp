#include "netconsole.h"

#include <net/net_device.h>
#include <lib/stdlib.h>

extern "C" {

/* What the `netconsole` command prints; crate::netconsole::Stats is the same
   struct. */
struct RustNetconsoleStats
{
    unsigned int Enabled;
    unsigned int DstIp;
    unsigned short DstPort;
    unsigned short SrcPort;
    unsigned int Attached;
    unsigned long Used;
    unsigned long Capacity;
    unsigned long Dropped;
    unsigned long Sent;
    unsigned long TxFailed;
    unsigned int Seq;
    unsigned long TailKeep;
    unsigned int Trimmed;
};

int rust_netconsole_setup();
int rust_netconsole_start(unsigned long dev);
void rust_netconsole_stop();
int rust_netconsole_enabled();
void rust_netconsole_log(const unsigned char* s, unsigned long len);
void rust_netconsole_panic_mark();
void rust_netconsole_panic_flush();
void rust_netconsole_stats(RustNetconsoleStats* out);

}

namespace Kernel
{

bool Netconsole::Setup()
{
    return rust_netconsole_setup() != 0;
}

bool Netconsole::Start(NetDevice* dev)
{
    if (dev == nullptr)
        return false;

    return rust_netconsole_start(reinterpret_cast<unsigned long>(dev)) == 0;
}

void Netconsole::Stop()
{
    rust_netconsole_stop();
}

bool Netconsole::IsEnabled()
{
    return rust_netconsole_enabled() != 0;
}

void Netconsole::Log(const char* s)
{
    if (s == nullptr)
        return;

    rust_netconsole_log(reinterpret_cast<const unsigned char*>(s), Stdlib::StrLen(s));
}

void Netconsole::PanicMark()
{
    rust_netconsole_panic_mark();
}

void Netconsole::PanicFlush()
{
    rust_netconsole_panic_flush();
}

void Netconsole::Dump(Stdlib::Printer& printer)
{
    RustNetconsoleStats stats = {};
    rust_netconsole_stats(&stats);

    if (stats.Enabled == 0)
    {
        printer.Printf("netconsole: disabled (boot with netconsole=ip:port)\n");
        return;
    }

    printer.Printf("netconsole: %u.%u.%u.%u:%u src port %u dev %s\n",
        (ulong)((stats.DstIp >> 24) & 0xFF), (ulong)((stats.DstIp >> 16) & 0xFF),
        (ulong)((stats.DstIp >> 8) & 0xFF), (ulong)(stats.DstIp & 0xFF),
        (ulong)stats.DstPort, (ulong)stats.SrcPort,
        stats.Attached ? "attached" : "none");
    printer.Printf("  buffered %u/%u bytes, dropped %u msgs, sent %u pkts, tx failed %u\n",
        stats.Used, stats.Capacity, stats.Dropped, stats.Sent, stats.TxFailed);
    printer.Printf("  next seq %u, backlog cap %u bytes%s\n",
        (ulong)stats.Seq, stats.TailKeep, stats.Trimmed ? ", applied" : "");
}

}
