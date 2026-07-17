#include <hal/pci.h>

#include "board.h"

#include <drivers/pci.h>
#include <kernel/trace.h>
#include <kernel/panic.h>
#include <mm/page_table.h>
#include <mm/memory_map.h>

/* arm64 PCI config backend: ECAM (pci-host-ecam-generic on QEMU virt).
   The ECAM window (256 MiB) sits above the premapped device GiB, so the
   buses we touch are mapped on demand. Bare -kernel boot has no firmware,
   so PciAssignResources probes and assigns BARs from the host bridge's
   32-bit MMIO window (CPU addr == PCI addr on QEMU virt). */

namespace
{

ulong EcamVa;          /* mapped base of the ECAM window (bus 0..N) */
ulong EcamBusesMapped; /* number of buses mapped from EcamVa */

ulong Mmio32Next, Mmio32End;   /* bump allocator: 32-bit non-prefetch window */
ulong Mmio64Next, Mmio64End;   /* bump allocator: 64-bit window */

ulong ConfigVa(u16 bus, u16 slot, u16 func, u16 offset)
{
    /* ECAM: addr = base + (bus << 20 | dev << 15 | func << 12) + offset.
       ECAM config space is Device memory: only aligned 32-bit accesses are
       allowed, so the raw accessor reads the dword containing `offset`
       (offset & 0xFFC) and the byte/word helpers extract the sub-field. */
    return EcamVa + (((ulong)bus << 20) | ((ulong)slot << 15) |
                     ((ulong)func << 12) | (offset & 0xFFC));
}

}

namespace Hal
{

u32 PciConfigRead32(u16 bus, u16 slot, u16 func, u16 offset)
{
    if (EcamVa == 0 || bus >= EcamBusesMapped)
        return 0xFFFFFFFF;
    return *reinterpret_cast<volatile u32*>(ConfigVa(bus, slot, func, offset));
}

void PciConfigWrite32(u16 bus, u16 slot, u16 func, u16 offset, u32 value)
{
    if (EcamVa == 0 || bus >= EcamBusesMapped)
        return;
    *reinterpret_cast<volatile u32*>(ConfigVa(bus, slot, func, offset)) = value;
}

/* Probe and assign one BAR; returns the number of BAR slots consumed
   (2 for a 64-bit BAR, 1 otherwise). Only 32-bit-window memory BARs are
   assigned; I/O BARs are disabled. */
static u8 AssignBar(Pci& pci, Pci::DeviceInfo* d, u8 bar)
{
    u16 off = (u16)(0x10 + bar * 4);
    u32 origLow = pci.ReadDword(d->Bus, d->Slot, d->Func, off);

    if (origLow & 1)
        return 1; /* I/O BAR — leave disabled */

    bool is64 = ((origLow & 0x6) == 0x4);
    u32 origHigh = is64 ? pci.ReadDword(d->Bus, d->Slot, d->Func, (u16)(off + 4)) : 0;

    /* Probe size across both halves for 64-bit BARs */
    pci.WriteDword(d->Bus, d->Slot, d->Func, off, 0xFFFFFFFF);
    u32 sizeLow = pci.ReadDword(d->Bus, d->Slot, d->Func, off);
    u32 sizeHigh = 0;
    if (is64)
    {
        pci.WriteDword(d->Bus, d->Slot, d->Func, (u16)(off + 4), 0xFFFFFFFF);
        sizeHigh = pci.ReadDword(d->Bus, d->Slot, d->Func, (u16)(off + 4));
    }
    pci.WriteDword(d->Bus, d->Slot, d->Func, off, origLow);
    if (is64)
        pci.WriteDword(d->Bus, d->Slot, d->Func, (u16)(off + 4), origHigh);

    /* Unimplemented BAR reads back 0 in its low mask */
    if ((sizeLow & ~0xFU) == 0 && (!is64 || sizeHigh == 0))
        return is64 ? 2 : 1;

    ulong sizeMask = is64 ? (((ulong)sizeHigh << 32) | (sizeLow & ~0xFU))
                          : (0xFFFFFFFF00000000ULL | (sizeLow & ~0xFU));
    ulong size = ~sizeMask + 1;
    if (size == 0)
        return is64 ? 2 : 1;

    /* 64-bit BARs go in the 64-bit window; 32-bit BARs in the 32-bit window */
    ulong* next = is64 ? &Mmio64Next : &Mmio32Next;
    ulong end = is64 ? Mmio64End : Mmio32End;

    ulong addr = (*next + (size - 1)) & ~(size - 1); /* natural alignment */
    if (addr + size > end)
    {
        Trace(0, "PciAssign: window exhausted for %u:%u.%u bar%u size 0x%p",
            (ulong)d->Bus, (ulong)d->Slot, (ulong)d->Func, (ulong)bar, size);
        return is64 ? 2 : 1;
    }
    *next = addr + size;

    pci.WriteDword(d->Bus, d->Slot, d->Func, off,
        (u32)(addr & 0xFFFFFFF0) | (origLow & 0xF));
    if (is64)
        pci.WriteDword(d->Bus, d->Slot, d->Func, (u16)(off + 4), (u32)(addr >> 32));

    Trace(0, "PciAssign: %u:%u.%u bar%u -> 0x%p size 0x%p %s",
        (ulong)d->Bus, (ulong)d->Slot, (ulong)d->Func, (ulong)bar, addr, size,
        is64 ? "64" : "32");

    return is64 ? 2 : 1;
}

void PciAssignResources(void* devices, ulong count, ulong stride)
{
    auto& board = Kernel::Board::GetInstance();
    if (board.EcamBase == 0)
        return;

    Mmio32Next = board.PciMmio32Base;
    Mmio32End = board.PciMmio32Base + board.PciMmio32Size;
    Mmio64Next = board.PciMmio64Base;
    Mmio64End = board.PciMmio64Base + board.PciMmio64Size;

    auto& pci = Pci::GetInstance();
    u8* base = static_cast<u8*>(devices);

    for (ulong i = 0; i < count; i++)
    {
        auto* d = reinterpret_cast<Pci::DeviceInfo*>(base + i * stride);
        if (!d->Valid)
            continue;
        if (d->Class == Pci::ClsBridgeDevice)
            continue; /* host/PCI bridges have no assignable device BARs here */

        /* Header type 0 has BAR0..5 */
        for (u8 bar = 0; bar < 6; )
            bar += AssignBar(pci, d, bar);

        /* Program the INTx routing (firmware's job; bare -kernel must do it).
           QEMU virt's interrupt-map swizzle: for INTx pin p (1..4) on device
           slot s, SPI = 3 + ((s + p - 1) % 4), i.e. GIC INTID 35..38. Write
           it into the config-space InterruptLine so drivers register level
           IRQs on the routed INTID unchanged. */
        u8 pin = pci.ReadByte(d->Bus, d->Slot, d->Func, 0x3D);
        if (pin >= 1 && pin <= 4)
        {
            u8 intId = (u8)(32 + 3 + ((d->Slot + pin - 1) % 4));
            pci.WriteByte(d->Bus, d->Slot, d->Func, 0x3C, intId);
            d->InterruptLine = intId;
            Trace(0, "PciIntx: %u:%u.%u pin %u -> intid %u",
                (ulong)d->Bus, (ulong)d->Slot, (ulong)d->Func, (ulong)pin, (ulong)intId);
        }

        /* Enable memory space + bus mastering */
        u16 cmd = pci.ReadWord(d->Bus, d->Slot, d->Func, 0x04);
        cmd |= (1 << 1) | (1 << 2);
        pci.WriteWord(d->Bus, d->Slot, d->Func, 0x04, cmd);
    }
}

}

namespace Kernel
{

/* Map the ECAM window (bus 0 is enough for QEMU virt's flat topology, but
   map a few buses to be safe) and record the MMIO window. Called from the
   arm64 boot path before Pci::Scan. */
bool PciEcamSetup()
{
    auto& board = Board::GetInstance();
    if (board.EcamBase == 0)
        return false;

    ulong buses = (ulong)board.PciBusEnd - board.PciBusStart + 1;
    if (buses > 4)
        buses = 4; /* map the first 4 buses (4 MiB) */

    ulong mapBytes = buses << 20;
    ulong va = Mm::PageTable::GetInstance().MapMmioRegion(board.EcamBase, mapBytes);
    if (va == 0)
    {
        Trace(0, "PciEcamSetup: ECAM map failed");
        return false;
    }

    EcamVa = va;
    EcamBusesMapped = buses;
    Trace(0, "PciEcamSetup: ecam 0x%p va 0x%p buses %u mmio32 0x%p+0x%p",
        board.EcamBase, va, buses, board.PciMmio32Base, board.PciMmio32Size);
    return true;
}

}
