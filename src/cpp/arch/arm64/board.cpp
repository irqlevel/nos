#include "board.h"
#include "fdt.h"

#include <lib/stdlib.h>
#include <mm/memory_map.h>

namespace Kernel
{

static_assert(Board::LinearMapReach == 0 - Mm::MemoryMap::KernelSpaceBase,
    "the linear map reaches from KernelSpaceBase to the top of the address space");

namespace
{

/* The first INTID the GIC gives each kind of interrupt */
const u32 PpiBase = 16;
const u32 SpiBase = 32;
/* The table interrupt_arm64.cpp dispatches from, and the u8 registration
   takes an INTID in */
const u32 TakenIntIds = 256;

/* interrupts = <type number flags>: type 0 = SPI (INTID 32+), 1 = PPI
   (INTID 16+). The triple at index; false if the value has no such triple
   or it names an interrupt the kernel cannot take. */
bool GicIntId(const Fdt::Prop& prop, ulong index, u32& intId)
{
    static const ulong CellsPerInterrupt = 3;
    static const u32 TypeSpi = 0;
    static const u32 TypePpi = 1;
    static const u32 PpiCount = 16;

    /* The whole triple, as #interrupt-cells = <3> has it */
    u32 type, num;
    if ((index + 1) * CellsPerInterrupt > prop.Length() / 4 ||
        !prop.Cell(index * CellsPerInterrupt, type) || !prop.Cell(index * CellsPerInterrupt + 1, num))
        return false;

    if (type == TypeSpi && num < TakenIntIds - SpiBase)
        intId = SpiBase + num;
    else if (type == TypePpi && num < PpiCount)
        intId = PpiBase + num;
    else
        return false;

    return Board::IsTakenIntId(intId);
}

bool NameStartsWith(const char* name, const char* prefix)
{
    return Stdlib::StrStr(name, prefix) == name;
}

}

bool Board::IsTakenIntId(u32 intId)
{
    return intId >= PpiBase && intId < TakenIntIds;
}

bool Board::Reachable(u64 base, u64 size)
{
    if (base < LinearMapReach && size <= LinearMapReach - base)
        return true;

    Refused++;
    return false;
}

bool Board::ReadWindow(const Fdt::Prop& reg, ulong index, u32 ac, u32 sc, u64& base, u64& size)
{
    if (!reg.Present())
        return false;

    if (!reg.Cells(index, ac, base) || !reg.Cells(index + ac, sc, size))
    {
        Refused++;
        return false;
    }

    return Reachable(base, size);
}

bool Board::Setup(void* dtbVa)
{
    Fdt fdt;

    if (fdt.Setup(dtbVa))
    {
        DtbRegion.Addr = (ulong)dtbVa;
        DtbRegion.Size = fdt.GetTotalSize();

        Fdt::Node node = {};
        while (fdt.NextNode(node))
        {
            const u32 ac = node.AddressCells;
            const u32 sc = node.SizeCells;

            if (NameStartsWith(node.Name, "memory@") ||
                Stdlib::StrCmp(node.Name, "memory") == 0)
            {
                /* reg = <address size>... in the parent's cells; the memory
                   map keeps what of each region can be RAM */
                Fdt::Prop reg = fdt.GetProp(node, "reg");
                const ulong entry = (ulong)ac + sc;
                if (reg.Present() && (entry == 0 || reg.Length() % (entry * 4) != 0))
                    Refused++;
                u64 addr, size;
                for (ulong i = 0; entry != 0 && MemRegionCount < MaxMemRegions &&
                     i < reg.Length() / 4 / entry; i++)
                {
                    if (!reg.Cells(i * entry, ac, addr) || !reg.Cells(i * entry + ac, sc, size))
                    {
                        Refused++;
                        break;
                    }
                    MemRegions[MemRegionCount].Addr = addr;
                    MemRegions[MemRegionCount].Size = size;
                    MemRegionCount++;
                }
            }
            else if (Stdlib::StrCmp(node.Name, "chosen") == 0)
            {
                Fdt::Prop prop = fdt.GetProp(node, "bootargs");
                const char* args = prop.String();
                if (args != nullptr)
                    Stdlib::StrnCpy(BootArgs, args, sizeof(BootArgs));
                else if (prop.Present())
                    Refused++;
            }
            else if (fdt.IsCompatible(node, "arm,psci-1.0") ||
                     fdt.IsCompatible(node, "arm,psci-0.2"))
            {
                const char* method = fdt.GetProp(node, "method").String();
                if (method != nullptr)
                    PsciUseHvc = (Stdlib::StrCmp(method, "hvc") == 0);
            }
            else if (fdt.IsCompatible(node, "arm,gic-v3-its"))
            {
                u64 base, size;
                if (ReadWindow(fdt.GetProp(node, "reg"), 0, ac, sc, base, size))
                    ItsBase = base;
            }
            else if (fdt.IsCompatible(node, "pci-host-ecam-generic"))
            {
                /* reg = ECAM config window (address-cells/size-cells of the
                   parent, i.e. 2/2 here) */
                u64 base, size;
                if (ReadWindow(fdt.GetProp(node, "reg"), 0, ac, sc, base, size))
                {
                    EcamBase = base;
                    EcamSize = size;
                }
                /* bus-range = <start end> */
                static const u32 MaxBus = 0xFF;
                Fdt::Prop br = fdt.GetProp(node, "bus-range");
                u32 start, end;
                if (br.Cell(0, start) && br.Cell(1, end))
                {
                    if (start <= end && end <= MaxBus)
                    {
                        PciBusStart = (u8)start;
                        PciBusEnd = (u8)end;
                    }
                    else
                    {
                        Refused++;
                    }
                }
                /* ranges: entries of <pci-addr(3) cpu-addr(2) size(2)>; the
                   host node's own #address-cells is 3, #size-cells 2. Pick
                   the 32-bit non-prefetch MMIO window (hi cell bits 25:24 =
                   0b10 -> space code 2). */
                static const ulong PciAddrCells = 3;
                static const ulong EntryCells = PciAddrCells + 2 + 2;
                Fdt::Prop ranges = fdt.GetProp(node, "ranges");
                for (ulong e = 0; e < ranges.Length() / 4 / EntryCells; e++)
                {
                    u32 hi;
                    u64 cpu, size;
                    if (!ranges.Cell(e * EntryCells, hi) ||
                        !ranges.Cells(e * EntryCells + PciAddrCells, 2, cpu) ||
                        !ranges.Cells(e * EntryCells + PciAddrCells + 2, 2, size))
                        break;

                    u32 space = (hi >> 24) & 3;
                    if (space != 2 && space != 3)
                        continue;
                    if (!Reachable(cpu, size))
                        continue;
                    if (space == 2) /* 32-bit MMIO */
                    {
                        PciMmio32Base = cpu;
                        PciMmio32Size = size;
                    }
                    else /* 64-bit MMIO */
                    {
                        PciMmio64Base = cpu;
                        PciMmio64Size = size;
                    }
                }
            }
            else if (fdt.IsCompatible(node, "arm,gic-v3"))
            {
                /* reg = <distributor> <redistributors> */
                Fdt::Prop reg = fdt.GetProp(node, "reg");
                u64 dist, distSize, redist, redistSize;
                if (ReadWindow(reg, 0, ac, sc, dist, distSize) &&
                    ReadWindow(reg, (ulong)ac + sc, ac, sc, redist, redistSize))
                {
                    GicdBase = dist;
                    GicdSize = distSize;
                    GicrBase = redist;
                    GicrSize = redistSize;
                }
            }
            else if (fdt.IsCompatible(node, "arm,pl011") || fdt.IsCompatible(node, "arm,pl031"))
            {
                const bool uart = fdt.IsCompatible(node, "arm,pl011");
                u64 base, size;
                if (ReadWindow(fdt.GetProp(node, "reg"), 0, ac, sc, base, size))
                    (uart ? Pl011Base : Pl031Base) = base;
                Fdt::Prop irq = fdt.GetProp(node, "interrupts");
                u32 intId;
                if (irq.Present())
                {
                    if (GicIntId(irq, 0, intId))
                        (uart ? Pl011IntId : Pl031IntId) = intId;
                    else
                        Refused++;
                }
            }
            else if (fdt.IsCompatible(node, "virtio,mmio"))
            {
                Fdt::Prop reg = fdt.GetProp(node, "reg");
                Fdt::Prop irq = fdt.GetProp(node, "interrupts");
                u64 base, size;
                u32 intId = 0;
                /* A device whose interrupt cannot be taken is no device: its
                   driver would wait on it for good */
                bool usable = !irq.Present() || GicIntId(irq, 0, intId);
                if (!usable)
                    Refused++;
                if (usable && VirtioMmioCount < MaxVirtioMmio && ReadWindow(reg, 0, ac, sc, base, size))
                {
                    auto& dev = VirtioMmio[VirtioMmioCount];
                    dev.Base = base;
                    dev.Size = size;
                    dev.IntId = intId;
                    VirtioMmioCount++;
                }
            }
            else if (fdt.IsCompatible(node, "arm,armv8-timer") ||
                     fdt.IsCompatible(node, "arm,armv7-timer"))
            {
                /* interrupts: sec-phys, phys, virt, hyp-phys (3 cells each);
                   we use the virtual timer (index 2). */
                static const ulong VirtualTimer = 2;
                Fdt::Prop irq = fdt.GetProp(node, "interrupts");
                u32 intId;
                if (irq.Present())
                {
                    if (GicIntId(irq, VirtualTimer, intId))
                        TimerIntId = intId;
                    else
                        Refused++;
                }
            }
            else if (NameStartsWith(node.Name, "cpu@") && node.Depth == 2)
            {
                Fdt::Prop reg = fdt.GetProp(node, "reg");
                u64 mpidr;
                if (reg.Cells(0, ac, mpidr))
                {
                    if (CpuCount < MaxBoardCpus)
                        CpuMpidr[CpuCount++] = mpidr;
                }
                else if (reg.Present())
                {
                    Refused++;
                }
            }
        }
    }

    ApplyFallbacks();
    return true;
}

void Board::ApplyFallbacks()
{
    /* QEMU virt machine defaults */
    if (MemRegionCount == 0)
    {
        MemRegions[0].Addr = 0x40000000;
        MemRegions[0].Size = 128 * Const::MB;
        MemRegionCount = 1;
    }
    if (GicdBase == 0)
    {
        GicdBase = 0x08000000;
        GicdSize = 0x10000;
        GicrBase = 0x080A0000;
        GicrSize = 0xF60000;
    }
    /* Each on its own: a device the tree placed but whose interrupt it did
       not give -- or gave as one the kernel cannot take -- would otherwise
       be set up on INTID 0, an SGI */
    if (Pl011Base == 0)
        Pl011Base = 0x09000000;
    if (Pl011IntId == 0)
        Pl011IntId = 33;
    if (Pl031Base == 0)
        Pl031Base = 0x09010000;
    if (Pl031IntId == 0)
        Pl031IntId = 34;
    if (TimerIntId == 0)
        TimerIntId = 27;
    if (ItsBase == 0)
        ItsBase = 0x08080000;
    if (EcamBase == 0)
    {
        EcamBase = 0x4010000000;
        EcamSize = 0x10000000;
        PciMmio32Base = 0x10000000;
        PciMmio32Size = 0x2eff0000;
        PciMmio64Base = 0x8000000000;
        PciMmio64Size = 0x8000000000;
        PciBusStart = 0;
        PciBusEnd = 0xff;
    }
    if (CpuCount == 0)
    {
        CpuMpidr[0] = 0;
        CpuCount = 1;
    }
}

}
