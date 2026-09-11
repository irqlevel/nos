#include "block_device.h"

#include <kernel/trace.h>
#include <kernel/spin_lock.h>
#include <lib/stdlib.h>

namespace Kernel
{

namespace
{

/* BlockDeviceTable's claims, kept here so the header, which half the kernel
   includes, stays free of the lock */
struct ClaimEntry
{
    BlockDevice* Dev;
    const char* Holder;
    ulong Handle;       /* 0: the slot is free */
};

struct ClaimTable
{
    static ClaimTable& GetInstance()
    {
        static ClaimTable Instance;
        return Instance;
    }

    ClaimTable()
        : Generation(0)
    {
        Stdlib::MemSet(Entries, 0, sizeof(Entries));
    }

    static const ulong MaxClaims = BlockDeviceTable::MaxDevices;
    /* A handle is the slot + 1 in its low bits and a count of claims above
       them, so a stale handle never releases the slot's next claim */
    static const ulong SlotBits = 8;
    static_assert(MaxClaims < (1UL << SlotBits), "a slot must fit a handle");

    SpinLock Lock;
    ClaimEntry Entries[MaxClaims];
    ulong Generation;
};

const char TooManyClaims[] = "too many claims already";

}

bool BlockDevice::InterruptsStarted = false;

void BlockDevice::SetInterruptsStarted()
{
    InterruptsStarted = true;
}

bool BlockDevice::GetInterruptsStarted()
{
    return InterruptsStarted;
}

BlockDeviceTable::BlockDeviceTable()
    : Count(0)
{
    for (ulong i = 0; i < MaxDevices; i++)
        Devices[i] = nullptr;
}

BlockDeviceTable::~BlockDeviceTable()
{
}

bool BlockDeviceTable::Overlap(BlockDevice* a, BlockDevice* b)
{
    for (BlockDevice* dev = a; dev != nullptr; dev = dev->GetParent())
    {
        if (dev == b)
            return true;
    }

    for (BlockDevice* dev = b; dev != nullptr; dev = dev->GetParent())
    {
        if (dev == a)
            return true;
    }

    return false;
}

ulong BlockDeviceTable::Claim(BlockDevice* dev, const char* holder, const char*& heldBy)
{
    auto& claims = ClaimTable::GetInstance();
    Stdlib::AutoLock lock(claims.Lock);

    ClaimEntry* slotEntry = nullptr;
    ulong slot = 0;
    for (ulong i = 0; i < ClaimTable::MaxClaims; i++)
    {
        ClaimEntry& entry = claims.Entries[i];
        if (entry.Handle == 0)
        {
            if (slotEntry == nullptr)
            {
                slotEntry = &entry;
                slot = i;
            }
        }
        else if (Overlap(entry.Dev, dev))
        {
            heldBy = entry.Holder;
            return 0;
        }
    }

    if (slotEntry == nullptr)
    {
        heldBy = TooManyClaims;
        return 0;
    }

    claims.Generation++;
    slotEntry->Dev = dev;
    slotEntry->Holder = holder;
    slotEntry->Handle = (claims.Generation << ClaimTable::SlotBits) | (slot + 1);
    return slotEntry->Handle;
}

void BlockDeviceTable::Release(ulong claim)
{
    const ulong index = claim & ((1UL << ClaimTable::SlotBits) - 1);
    if (index == 0 || index > ClaimTable::MaxClaims)
        return;

    auto& claims = ClaimTable::GetInstance();
    Stdlib::AutoLock lock(claims.Lock);

    ClaimEntry& entry = claims.Entries[index - 1];
    if (entry.Handle == claim)
    {
        entry.Handle = 0;
        entry.Dev = nullptr;
        entry.Holder = nullptr;
    }
}

bool BlockDeviceTable::Register(BlockDevice* dev)
{
    if (Count >= MaxDevices || dev == nullptr)
        return false;

    Devices[Count] = dev;
    Count++;

    Trace(0, "BlockDevice registered: %s capacity %u sectors",
        dev->GetName(), dev->GetCapacity());

    return true;
}

BlockDevice* BlockDeviceTable::Find(const char* name)
{
    for (ulong i = 0; i < Count; i++)
    {
        if (Devices[i] && Stdlib::StrCmp(Devices[i]->GetName(), name) == 0)
            return Devices[i];
    }
    return nullptr;
}

void BlockDeviceTable::Dump(Stdlib::Printer& printer)
{
    if (Count == 0)
    {
        printer.Printf("no block devices\n");
        return;
    }

    for (ulong i = 0; i < Count; i++)
    {
        if (!Devices[i])
            continue;

        u64 cap = Devices[i]->GetCapacity();
        u64 secSize = Devices[i]->GetSectorSize();
        u64 mb = (cap * secSize) / (1024 * 1024);

        printer.Printf("%s  %u sectors (%u MB)  %u bytes/sector\n",
            Devices[i]->GetName(), cap, mb, secSize);
    }
}

ulong BlockDeviceTable::GetCount()
{
    return Count;
}

BlockDevice* BlockDeviceTable::GetDevice(ulong index)
{
    if (index >= Count)
        return nullptr;
    return Devices[index];
}

}
