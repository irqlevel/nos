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
   so pci_assign_resources probes and assigns BARs from the host bridge's
   32-bit MMIO window (CPU addr == PCI addr on QEMU virt). */

namespace
{

ulong ecam_va;          /* mapped base of the ECAM window (bus 0..N) */
ulong ecam_buses_mapped; /* number of buses mapped from ecam_va */

ulong mmio32_next, mmio32_end;   /* bump allocator: 32-bit non-prefetch window */
ulong mmio64_next, mmio64_end;   /* bump allocator: 64-bit window */

ulong config_va(u16 bus, u16 slot, u16 func, u16 offset)
{
    /* ECAM: addr = base + (bus << 20 | dev << 15 | func << 12) + offset.
       ECAM config space is Device memory: only aligned 32-bit accesses are
       allowed, so the raw accessor reads the dword containing `offset`
       (offset & 0xFFC) and the byte/word helpers extract the sub-field. */
    return ecam_va + (((ulong)bus << 20) | ((ulong)slot << 15) |
                     ((ulong)func << 12) | (offset & 0xFFC));
}

}

namespace hal
{

u32 pci_config_read32(u16 bus, u16 slot, u16 func, u16 offset)
{
    if (ecam_va == 0 || bus >= ecam_buses_mapped)
        return 0xFFFFFFFF;
    return *reinterpret_cast<volatile u32*>(config_va(bus, slot, func, offset));
}

void pci_config_write32(u16 bus, u16 slot, u16 func, u16 offset, u32 value)
{
    if (ecam_va == 0 || bus >= ecam_buses_mapped)
        return;
    *reinterpret_cast<volatile u32*>(config_va(bus, slot, func, offset)) = value;
}

/* Probe and assign one BAR; returns the number of BAR slots consumed
   (2 for a 64-bit BAR, 1 otherwise). Only 32-bit-window memory BARs are
   assigned; I/O BARs are disabled. */
static u8 assign_bar(Pci& pci, Pci::DeviceInfo* d, u8 bar)
{
    u16 off = (u16)(0x10 + bar * 4);
    u32 orig_low = pci.read_dword(d->bus, d->slot, d->func, off);

    if (orig_low & 1)
        return 1; /* I/O BAR — leave disabled */

    bool is64 = ((orig_low & 0x6) == 0x4);
    u32 orig_high = is64 ? pci.read_dword(d->bus, d->slot, d->func, (u16)(off + 4)) : 0;

    /* Probe size across both halves for 64-bit BARs */
    pci.write_dword(d->bus, d->slot, d->func, off, 0xFFFFFFFF);
    u32 size_low = pci.read_dword(d->bus, d->slot, d->func, off);
    u32 size_high = 0;
    if (is64)
    {
        pci.write_dword(d->bus, d->slot, d->func, (u16)(off + 4), 0xFFFFFFFF);
        size_high = pci.read_dword(d->bus, d->slot, d->func, (u16)(off + 4));
    }
    pci.write_dword(d->bus, d->slot, d->func, off, orig_low);
    if (is64)
        pci.write_dword(d->bus, d->slot, d->func, (u16)(off + 4), orig_high);

    /* Unimplemented BAR reads back 0 in its low mask */
    if ((size_low & ~0xFU) == 0 && (!is64 || size_high == 0))
        return is64 ? 2 : 1;

    ulong size_mask = is64 ? (((ulong)size_high << 32) | (size_low & ~0xFU))
                          : (0xFFFFFFFF00000000ULL | (size_low & ~0xFU));
    ulong size = ~size_mask + 1;
    if (size == 0)
        return is64 ? 2 : 1;

    /* 64-bit BARs go in the 64-bit window; 32-bit BARs in the 32-bit window */
    ulong* next = is64 ? &mmio64_next : &mmio32_next;
    ulong end = is64 ? mmio64_end : mmio32_end;

    ulong addr = (*next + (size - 1)) & ~(size - 1); /* natural alignment */
    if (addr + size > end)
    {
        trace(0, "PciAssign: window exhausted for %lu:%lu.%lu bar%lu size 0x%lX",
            (ulong)d->bus, (ulong)d->slot, (ulong)d->func, (ulong)bar, size);
        return is64 ? 2 : 1;
    }
    *next = addr + size;

    pci.write_dword(d->bus, d->slot, d->func, off,
        (u32)(addr & 0xFFFFFFF0) | (orig_low & 0xF));
    if (is64)
        pci.write_dword(d->bus, d->slot, d->func, (u16)(off + 4), (u32)(addr >> 32));

    trace(0, "PciAssign: %lu:%lu.%lu bar%lu -> 0x%lX size 0x%lX %s",
        (ulong)d->bus, (ulong)d->slot, (ulong)d->func, (ulong)bar, addr, size,
        is64 ? "64" : "32");

    return is64 ? 2 : 1;
}

void pci_assign_resources(void* devices, ulong count, ulong stride)
{
    auto& board = kernel::Board::get_instance();
    if (board.ecam_base_ == 0)
        return;

    mmio32_next = board.pci_mmio32_base_;
    mmio32_end = board.pci_mmio32_base_ + board.pci_mmio32_size_;
    mmio64_next = board.pci_mmio64_base_;
    mmio64_end = board.pci_mmio64_base_ + board.pci_mmio64_size_;

    auto& pci = Pci::get_instance();
    u8* base = static_cast<u8*>(devices);

    for (ulong i = 0; i < count; i++)
    {
        auto* d = reinterpret_cast<Pci::DeviceInfo*>(base + i * stride);
        if (!d->valid)
            continue;
        if (d->base_class == Pci::CLS_BRIDGE_DEVICE)
            continue; /* host/PCI bridges have no assignable device BARs here */

        /* Header type 0 has BAR0..5 */
        for (u8 bar = 0; bar < 6; )
            bar += assign_bar(pci, d, bar);

        /* Program the INTx routing (firmware's job; bare -kernel must do it).
           QEMU virt's interrupt-map swizzle: for INTx pin p (1..4) on device
           slot s, SPI = 3 + ((s + p - 1) % 4), i.e. GIC INTID 35..38. Write
           it into the config-space interrupt_line so drivers register level
           IRQs on the routed INTID unchanged. */
        u8 pin = pci.read_byte(d->bus, d->slot, d->func, 0x3D);
        if (pin >= 1 && pin <= 4)
        {
            u8 int_id = (u8)(32 + 3 + ((d->slot + pin - 1) % 4));
            pci.write_byte(d->bus, d->slot, d->func, 0x3C, int_id);
            d->interrupt_line = int_id;
            trace(0, "PciIntx: %lu:%lu.%lu pin %lu -> intid %lu",
                (ulong)d->bus, (ulong)d->slot, (ulong)d->func, (ulong)pin, (ulong)int_id);
        }

        /* Enable memory space + bus mastering */
        u16 cmd = pci.read_word(d->bus, d->slot, d->func, 0x04);
        cmd |= (1 << 1) | (1 << 2);
        pci.write_word(d->bus, d->slot, d->func, 0x04, cmd);
    }
}

}

namespace kernel
{

/* Map the ECAM window (bus 0 is enough for QEMU virt's flat topology, but
   map a few buses to be safe) and record the MMIO window. Called from the
   arm64 boot path before Pci::scan. */
bool pci_ecam_setup()
{
    auto& board = Board::get_instance();
    if (board.ecam_base_ == 0)
        return false;

    ulong buses = (ulong)board.pci_bus_end_ - board.pci_bus_start_ + 1;
    if (buses > 4)
        buses = 4; /* map the first 4 buses (4 MiB) */

    ulong map_bytes = buses << 20;
    ulong va = mm::PageTable::get_instance().map_mmio_region(board.ecam_base_, map_bytes);
    if (va == 0)
    {
        trace(0, "pci_ecam_setup: ECAM map failed");
        return false;
    }

    ecam_va = va;
    ecam_buses_mapped = buses;
    trace(0, "pci_ecam_setup: ecam 0x%lX va 0x%lX buses %lu mmio32 0x%lX+0x%lX",
        board.ecam_base_, va, buses, board.pci_mmio32_base_, board.pci_mmio32_size_);
    return true;
}

}
