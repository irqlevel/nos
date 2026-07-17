#include "board.h"
#include "fdt.h"

#include <lib/stdlib.h>

namespace Kernel
{

namespace
{

/* interrupts = <type number flags>: type 0 = SPI (INTID 32+), 1 = PPI
   (INTID 16+). */
u32 GicIntId(const void* prop)
{
    u32 type = Fdt::Be32(prop);
    u32 num = Fdt::Be32(static_cast<const u8*>(prop) + 4);
    return (type == 0) ? (32 + num) : (16 + num);
}

bool NameStartsWith(const char* name, const char* prefix)
{
    return Stdlib::StrStr(name, prefix) == name;
}

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
            u32 len;

            if (NameStartsWith(node.Name, "memory@") ||
                Stdlib::StrCmp(node.Name, "memory") == 0)
            {
                const void* reg = fdt.GetProp(node, "reg", len);
                ulong group = (node.AddressCells + node.SizeCells) * 4;
                if (reg != nullptr && group != 0)
                {
                    ulong count = len / group;
                    for (ulong i = 0; i < count && MemRegionCount < MaxMemRegions; i++)
                    {
                        const u8* p = static_cast<const u8*>(reg) + i * group;
                        MemRegions[MemRegionCount].Addr = Fdt::ReadCells(p, 0, node.AddressCells);
                        MemRegions[MemRegionCount].Size =
                            Fdt::ReadCells(static_cast<const u8*>(p) + node.AddressCells * 4,
                                           0, node.SizeCells);
                        MemRegionCount++;
                    }
                }
            }
            else if (Stdlib::StrCmp(node.Name, "chosen") == 0)
            {
                const char* args = fdt.GetPropString(node, "bootargs");
                if (args != nullptr)
                    Stdlib::StrnCpy(BootArgs, args, sizeof(BootArgs) - 1);
            }
            else if (fdt.IsCompatible(node, "arm,psci-1.0") ||
                     fdt.IsCompatible(node, "arm,psci-0.2"))
            {
                const char* method = fdt.GetPropString(node, "method");
                if (method != nullptr)
                    PsciUseHvc = (Stdlib::StrCmp(method, "hvc") == 0);
            }
            else if (fdt.IsCompatible(node, "arm,gic-v3-its"))
            {
                const void* reg = fdt.GetProp(node, "reg", len);
                if (reg != nullptr)
                    ItsBase = Fdt::ReadCells(reg, 0, node.AddressCells);
            }
            else if (fdt.IsCompatible(node, "pci-host-ecam-generic"))
            {
                /* reg = ECAM config window (address-cells/size-cells of the
                   parent, i.e. 2/2 here) */
                const void* reg = fdt.GetProp(node, "reg", len);
                if (reg != nullptr)
                {
                    EcamBase = Fdt::ReadCells(reg, 0, node.AddressCells);
                    EcamSize = Fdt::ReadCells(
                        static_cast<const u8*>(reg) + node.AddressCells * 4,
                        0, node.SizeCells);
                }
                /* bus-range = <start end> */
                const void* br = fdt.GetProp(node, "bus-range", len);
                if (br != nullptr && len >= 8)
                {
                    PciBusStart = (u8)Fdt::Be32(br);
                    PciBusEnd = (u8)Fdt::Be32(static_cast<const u8*>(br) + 4);
                }
                /* ranges: entries of <pci-addr(3) cpu-addr(2) size(2)>; the
                   host node's own #address-cells is 3, #size-cells 2. Pick
                   the 32-bit non-prefetch MMIO window (hi cell bits 25:24 =
                   0b10 -> space code 2). */
                const void* ranges = fdt.GetProp(node, "ranges", len);
                if (ranges != nullptr)
                {
                    const u8* p = static_cast<const u8*>(ranges);
                    ulong entry = (3 + 2 + 2) * 4;
                    ulong count = len / entry;
                    for (ulong e = 0; e < count; e++)
                    {
                        const u8* r = p + e * entry;
                        u32 hi = Fdt::Be32(r);
                        u32 space = (hi >> 24) & 3;
                        if (space == 2) /* 32-bit MMIO */
                        {
                            PciMmio32Base = Fdt::ReadCells(r + 12, 0, 2); /* cpu addr */
                            PciMmio32Size = Fdt::ReadCells(r + 20, 0, 2); /* size */
                        }
                        else if (space == 3) /* 64-bit MMIO */
                        {
                            PciMmio64Base = Fdt::ReadCells(r + 12, 0, 2);
                            PciMmio64Size = Fdt::ReadCells(r + 20, 0, 2);
                        }
                    }
                }
            }
            else if (fdt.IsCompatible(node, "arm,gic-v3"))
            {
                const void* reg = fdt.GetProp(node, "reg", len);
                if (reg != nullptr)
                {
                    ulong ac = node.AddressCells, sc = node.SizeCells;
                    const u8* p = static_cast<const u8*>(reg);
                    GicdBase = Fdt::ReadCells(p, 0, ac);
                    GicdSize = Fdt::ReadCells(p + ac * 4, 0, sc);
                    GicrBase = Fdt::ReadCells(p + (ac + sc) * 4, 0, ac);
                    GicrSize = Fdt::ReadCells(p + (2 * ac + sc) * 4, 0, sc);
                }
            }
            else if (fdt.IsCompatible(node, "arm,pl011"))
            {
                const void* reg = fdt.GetProp(node, "reg", len);
                if (reg != nullptr)
                    Pl011Base = Fdt::ReadCells(reg, 0, node.AddressCells);
                const void* irq = fdt.GetProp(node, "interrupts", len);
                if (irq != nullptr && len >= 12)
                    Pl011IntId = GicIntId(irq);
            }
            else if (fdt.IsCompatible(node, "arm,pl031"))
            {
                const void* reg = fdt.GetProp(node, "reg", len);
                if (reg != nullptr)
                    Pl031Base = Fdt::ReadCells(reg, 0, node.AddressCells);
                const void* irq = fdt.GetProp(node, "interrupts", len);
                if (irq != nullptr && len >= 12)
                    Pl031IntId = GicIntId(irq);
            }
            else if (fdt.IsCompatible(node, "virtio,mmio"))
            {
                if (VirtioMmioCount < MaxVirtioMmio)
                {
                    const void* reg = fdt.GetProp(node, "reg", len);
                    const void* irq = fdt.GetProp(node, "interrupts", len);
                    if (reg != nullptr)
                    {
                        auto& dev = VirtioMmio[VirtioMmioCount];
                        dev.Base = Fdt::ReadCells(reg, 0, node.AddressCells);
                        dev.Size = Fdt::ReadCells(
                            static_cast<const u8*>(reg) + node.AddressCells * 4,
                            0, node.SizeCells);
                        dev.IntId = (irq != nullptr) ? GicIntId(irq) : 0;
                        VirtioMmioCount++;
                    }
                }
            }
            else if (fdt.IsCompatible(node, "arm,armv8-timer") ||
                     fdt.IsCompatible(node, "arm,armv7-timer"))
            {
                /* interrupts: sec-phys, phys, virt, hyp-phys (3 cells each);
                   we use the virtual timer (index 2). */
                const void* irq = fdt.GetProp(node, "interrupts", len);
                if (irq != nullptr && len >= 36)
                    TimerIntId = GicIntId(static_cast<const u8*>(irq) + 24);
            }
            else if (NameStartsWith(node.Name, "cpu@") && node.Depth == 2)
            {
                const void* reg = fdt.GetProp(node, "reg", len);
                if (reg != nullptr && CpuCount < MaxBoardCpus)
                    CpuMpidr[CpuCount++] = Fdt::ReadCells(reg, 0, node.AddressCells);
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
    if (Pl011Base == 0)
    {
        Pl011Base = 0x09000000;
        Pl011IntId = 33;
    }
    if (Pl031Base == 0)
    {
        Pl031Base = 0x09010000;
        Pl031IntId = 34;
    }
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
