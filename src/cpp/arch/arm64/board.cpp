#include "board.h"
#include "fdt.h"

#include <lib/stdlib.h>
#include <mm/memory_map.h>

namespace kernel
{

static_assert(Board::LINEAR_MAP_REACH == 0 - mm::MemoryMap::KERNEL_SPACE_BASE,
    "the linear map reaches from KERNEL_SPACE_BASE to the top of the address space");

namespace
{

/* The first INTID the GIC gives each kind of interrupt */
const u32 PPI_BASE = 16;
const u32 SPI_BASE = 32;
/* The table interrupt_arm64.cpp dispatches from, and the u8 registration
   takes an INTID in */
const u32 TAKEN_INT_IDS = 256;

/* interrupts = <type number flags>: type 0 = SPI (INTID 32+), 1 = PPI
   (INTID 16+). The triple at index; false if the value has no such triple
   or it names an interrupt the kernel cannot take. */
bool gic_int_id(const Fdt::Prop& prop, ulong index, u32& int_id)
{
    static const ulong CELLS_PER_INTERRUPT = 3;
    static const u32 TYPE_SPI = 0;
    static const u32 TYPE_PPI = 1;
    static const u32 PPI_COUNT = 16;

    /* The whole triple, as #interrupt-cells = <3> has it */
    u32 type, num;
    if ((index + 1) * CELLS_PER_INTERRUPT > prop.length() / 4 ||
        !prop.cell(index * CELLS_PER_INTERRUPT, type) || !prop.cell(index * CELLS_PER_INTERRUPT + 1, num))
        return false;

    if (type == TYPE_SPI && num < TAKEN_INT_IDS - SPI_BASE)
        int_id = SPI_BASE + num;
    else if (type == TYPE_PPI && num < PPI_COUNT)
        int_id = PPI_BASE + num;
    else
        return false;

    return Board::is_taken_int_id(int_id);
}

bool name_starts_with(const char* name, const char* prefix)
{
    return stdlib::str_str(name, prefix) == name;
}

}

bool Board::is_taken_int_id(u32 int_id)
{
    return int_id >= PPI_BASE && int_id < TAKEN_INT_IDS;
}

bool Board::reachable(u64 base, u64 size)
{
    if (base < LINEAR_MAP_REACH && size <= LINEAR_MAP_REACH - base)
        return true;

    refused_++;
    return false;
}

bool Board::read_window(const Fdt::Prop& reg, ulong index, u32 ac, u32 sc, u64& base, u64& size)
{
    if (!reg.present())
        return false;

    if (!reg.cells(index, ac, base) || !reg.cells(index + ac, sc, size))
    {
        refused_++;
        return false;
    }

    return reachable(base, size);
}

bool Board::setup(void* dtb_va)
{
    Fdt fdt;

    if (fdt.setup(dtb_va))
    {
        dtb_region_.addr = (ulong)dtb_va;
        dtb_region_.size = fdt.get_total_size();

        Fdt::Node node = {};
        while (fdt.next_node(node))
        {
            const u32 ac = node.address_cells;
            const u32 sc = node.size_cells;

            if (name_starts_with(node.name, "memory@") ||
                stdlib::str_cmp(node.name, "memory") == 0)
            {
                /* reg = <address size>... in the parent's cells; the memory
                   map keeps what of each region can be RAM */
                Fdt::Prop reg = fdt.get_prop(node, "reg");
                const ulong entry = (ulong)ac + sc;
                if (reg.present() && (entry == 0 || reg.length() % (entry * 4) != 0))
                    refused_++;
                u64 addr, size;
                for (ulong i = 0; entry != 0 && mem_region_count_ < MAX_MEM_REGIONS &&
                     i < reg.length() / 4 / entry; i++)
                {
                    if (!reg.cells(i * entry, ac, addr) || !reg.cells(i * entry + ac, sc, size))
                    {
                        refused_++;
                        break;
                    }
                    mem_regions_[mem_region_count_].addr = addr;
                    mem_regions_[mem_region_count_].size = size;
                    mem_region_count_++;
                }
            }
            else if (stdlib::str_cmp(node.name, "chosen") == 0)
            {
                Fdt::Prop prop = fdt.get_prop(node, "bootargs");
                const char* args = prop.string();
                if (args != nullptr)
                    stdlib::strn_cpy(boot_args_, args, sizeof(boot_args_));
                else if (prop.present())
                    refused_++;
            }
            else if (fdt.is_compatible(node, "arm,psci-1.0") ||
                     fdt.is_compatible(node, "arm,psci-0.2"))
            {
                const char* method = fdt.get_prop(node, "method").string();
                if (method != nullptr)
                    psci_use_hvc_ = (stdlib::str_cmp(method, "hvc") == 0);
            }
            else if (fdt.is_compatible(node, "arm,gic-v3-its"))
            {
                u64 base, size;
                if (read_window(fdt.get_prop(node, "reg"), 0, ac, sc, base, size))
                    its_base_ = base;
            }
            else if (fdt.is_compatible(node, "pci-host-ecam-generic"))
            {
                /* reg = ECAM config window (address-cells/size-cells of the
                   parent, i.e. 2/2 here) */
                u64 base, size;
                if (read_window(fdt.get_prop(node, "reg"), 0, ac, sc, base, size))
                {
                    ecam_base_ = base;
                    ecam_size_ = size;
                }
                /* bus-range = <start end> */
                static const u32 MAX_BUS = 0xFF;
                Fdt::Prop br = fdt.get_prop(node, "bus-range");
                u32 start, end;
                if (br.cell(0, start) && br.cell(1, end))
                {
                    if (start <= end && end <= MAX_BUS)
                    {
                        pci_bus_start_ = (u8)start;
                        pci_bus_end_ = (u8)end;
                    }
                    else
                    {
                        refused_++;
                    }
                }
                /* ranges: entries of <pci-addr(3) cpu-addr(2) size(2)>; the
                   host node's own #address-cells is 3, #size-cells 2. Pick
                   the 32-bit non-prefetch MMIO window (hi cell bits 25:24 =
                   0b10 -> space code 2). */
                static const ulong PCI_ADDR_CELLS = 3;
                static const ulong ENTRY_CELLS = PCI_ADDR_CELLS + 2 + 2;
                Fdt::Prop ranges = fdt.get_prop(node, "ranges");
                for (ulong e = 0; e < ranges.length() / 4 / ENTRY_CELLS; e++)
                {
                    u32 hi;
                    u64 cpu, size;
                    if (!ranges.cell(e * ENTRY_CELLS, hi) ||
                        !ranges.cells(e * ENTRY_CELLS + PCI_ADDR_CELLS, 2, cpu) ||
                        !ranges.cells(e * ENTRY_CELLS + PCI_ADDR_CELLS + 2, 2, size))
                        break;

                    u32 space = (hi >> 24) & 3;
                    if (space != 2 && space != 3)
                        continue;
                    if (!reachable(cpu, size))
                        continue;
                    if (space == 2) /* 32-bit MMIO */
                    {
                        pci_mmio32_base_ = cpu;
                        pci_mmio32_size_ = size;
                    }
                    else /* 64-bit MMIO */
                    {
                        pci_mmio64_base_ = cpu;
                        pci_mmio64_size_ = size;
                    }
                }
            }
            else if (fdt.is_compatible(node, "arm,gic-v3"))
            {
                /* reg = <distributor> <redistributors> */
                Fdt::Prop reg = fdt.get_prop(node, "reg");
                u64 dist, dist_size, redist, redist_size;
                if (read_window(reg, 0, ac, sc, dist, dist_size) &&
                    read_window(reg, (ulong)ac + sc, ac, sc, redist, redist_size))
                {
                    gicd_base_ = dist;
                    gicd_size_ = dist_size;
                    gicr_base_ = redist;
                    gicr_size_ = redist_size;
                }
            }
            else if (fdt.is_compatible(node, "arm,pl011") || fdt.is_compatible(node, "arm,pl031"))
            {
                const bool uart = fdt.is_compatible(node, "arm,pl011");
                u64 base, size;
                if (read_window(fdt.get_prop(node, "reg"), 0, ac, sc, base, size))
                    (uart ? pl011_base_ : pl031_base_) = base;
                Fdt::Prop irq = fdt.get_prop(node, "interrupts");
                u32 int_id;
                if (irq.present())
                {
                    if (gic_int_id(irq, 0, int_id))
                        (uart ? pl011_int_id_ : pl031_int_id_) = int_id;
                    else
                        refused_++;
                }
            }
            else if (fdt.is_compatible(node, "virtio,mmio"))
            {
                Fdt::Prop reg = fdt.get_prop(node, "reg");
                Fdt::Prop irq = fdt.get_prop(node, "interrupts");
                u64 base, size;
                u32 int_id = 0;
                /* A device whose interrupt cannot be taken is no device: its
                   driver would wait on it for good */
                bool usable = !irq.present() || gic_int_id(irq, 0, int_id);
                if (!usable)
                    refused_++;
                if (usable && virtio_mmio_count_ < MAX_VIRTIO_MMIO && read_window(reg, 0, ac, sc, base, size))
                {
                    auto& dev = virtio_mmio_[virtio_mmio_count_];
                    dev.base = base;
                    dev.size = size;
                    dev.int_id = int_id;
                    virtio_mmio_count_++;
                }
            }
            else if (fdt.is_compatible(node, "arm,armv8-timer") ||
                     fdt.is_compatible(node, "arm,armv7-timer"))
            {
                /* interrupts: sec-phys, phys, virt, hyp-phys (3 cells each);
                   we use the virtual timer (index 2). */
                static const ulong VIRTUAL_TIMER = 2;
                Fdt::Prop irq = fdt.get_prop(node, "interrupts");
                u32 int_id;
                if (irq.present())
                {
                    if (gic_int_id(irq, VIRTUAL_TIMER, int_id))
                        timer_int_id_ = int_id;
                    else
                        refused_++;
                }
            }
            else if (name_starts_with(node.name, "cpu@") && node.depth == 2)
            {
                Fdt::Prop reg = fdt.get_prop(node, "reg");
                u64 mpidr;
                if (reg.cells(0, ac, mpidr))
                {
                    if (cpu_count_ < MAX_BOARD_CPUS)
                        cpu_mpidr_[cpu_count_++] = mpidr;
                }
                else if (reg.present())
                {
                    refused_++;
                }
            }
        }
    }

    apply_fallbacks();
    return true;
}

void Board::apply_fallbacks()
{
    /* QEMU virt machine defaults */
    if (mem_region_count_ == 0)
    {
        mem_regions_[0].addr = 0x40000000;
        mem_regions_[0].size = 128 * consts::MB;
        mem_region_count_ = 1;
    }
    if (gicd_base_ == 0)
    {
        gicd_base_ = 0x08000000;
        gicd_size_ = 0x10000;
        gicr_base_ = 0x080A0000;
        gicr_size_ = 0xF60000;
    }
    /* Each on its own: a device the tree placed but whose interrupt it did
       not give -- or gave as one the kernel cannot take -- would otherwise
       be set up on INTID 0, an SGI */
    if (pl011_base_ == 0)
        pl011_base_ = 0x09000000;
    if (pl011_int_id_ == 0)
        pl011_int_id_ = 33;
    if (pl031_base_ == 0)
        pl031_base_ = 0x09010000;
    if (pl031_int_id_ == 0)
        pl031_int_id_ = 34;
    if (timer_int_id_ == 0)
        timer_int_id_ = 27;
    if (its_base_ == 0)
        its_base_ = 0x08080000;
    if (ecam_base_ == 0)
    {
        ecam_base_ = 0x4010000000;
        ecam_size_ = 0x10000000;
        pci_mmio32_base_ = 0x10000000;
        pci_mmio32_size_ = 0x2eff0000;
        pci_mmio64_base_ = 0x8000000000;
        pci_mmio64_size_ = 0x8000000000;
        pci_bus_start_ = 0;
        pci_bus_end_ = 0xff;
    }
    if (cpu_count_ == 0)
    {
        cpu_mpidr_[0] = 0;
        cpu_count_ = 1;
    }
}

}
