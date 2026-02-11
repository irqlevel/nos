#include "net_device.h"

#include <kernel/trace.h>
#include <lib/stdlib.h>

namespace Kernel
{

NetDeviceTable::NetDeviceTable()
    : Count(0)
{
    for (ulong i = 0; i < MaxDevices; i++)
        Devices[i] = nullptr;
}

NetDeviceTable::~NetDeviceTable()
{
}

bool NetDeviceTable::Register(NetDevice* dev)
{
    if (Count >= MaxDevices || dev == nullptr)
        return false;

    Devices[Count] = dev;
    Count++;

    u8 mac[6];
    dev->GetMac(mac);

    Trace(0, "NetDevice registered: %s mac %p:%p:%p:%p:%p:%p",
        dev->GetName(),
        (ulong)mac[0], (ulong)mac[1], (ulong)mac[2],
        (ulong)mac[3], (ulong)mac[4], (ulong)mac[5]);

    return true;
}

NetDevice* NetDeviceTable::Find(const char* name)
{
    for (ulong i = 0; i < Count; i++)
    {
        if (Devices[i] && Stdlib::StrCmp(Devices[i]->GetName(), name) == 0)
            return Devices[i];
    }
    return nullptr;
}

void NetDeviceTable::Dump(Stdlib::Printer& printer)
{
    if (Count == 0)
    {
        printer.Printf("no network devices\n");
        return;
    }

    for (ulong i = 0; i < Count; i++)
    {
        if (!Devices[i])
            continue;

        u8 mac[6];
        Devices[i]->GetMac(mac);

        printer.Printf("%s  %p:%p:%p:%p:%p:%p  tx:%u rx:%u drop:%u\n",
            Devices[i]->GetName(),
            (ulong)mac[0], (ulong)mac[1], (ulong)mac[2],
            (ulong)mac[3], (ulong)mac[4], (ulong)mac[5],
            Devices[i]->GetTxPackets(),
            Devices[i]->GetRxPackets(),
            Devices[i]->GetRxDropped());
    }
}

ulong NetDeviceTable::GetCount()
{
    return Count;
}

}
