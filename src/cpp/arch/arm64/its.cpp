#include "its.h"

#include <kernel/interrupt.h>
#include <kernel/trace.h>
#include <kernel/panic.h>
#include <hal/context.h>
#include <mm/new.h>
#include <mm/page_table.h>
#include <mm/memory_map.h>
#include <lib/stdlib.h>

namespace Kernel
{

namespace
{

/* ITS register offsets */
const ulong GitsCtlr = 0x0000;
const ulong GitsTyper = 0x0008;
const ulong GitsCbaser = 0x0080;
const ulong GitsCwriter = 0x0088;
const ulong GitsCreadr = 0x0090;
const ulong GitsBaser0 = 0x0100;
const ulong GitsTranslater = 0x10040;

/* Redistributor LPI registers (RD_base) */
const ulong GicrCtlr = 0x0000;
const ulong GicrTyper = 0x0008;
const ulong GicrPropbaser = 0x0070;
const ulong GicrPendbaser = 0x0078;

const u32 GitsCtlrEnabled = 1;
const u32 GicrCtlrEnableLpis = 1;

/* Command opcodes */
const u64 CmdSyncOp = 0x05;
const u64 CmdMapdOp = 0x08;
const u64 CmdMapcOp = 0x09;
const u64 CmdMaptiOp = 0x0A;
const u64 CmdInvOp = 0x0C;

/* Cacheability/shareability attributes. The field *positions* differ between
   register families:
   - GITS_BASER / GITS_CBASER: InnerCache[61:59], Shareability[11:10]
   - GICR_PROPBASER / GICR_PENDBASER: InnerCache[9:7], Shareability[11:10]
   Both set Normal Inner WB write-allocate + Inner shareable. Mixing a
   shareable + non-cacheable encoding (the bug of using the BASER layout for
   PROPBASER) wedges the redistributor. */
const u64 BaserCacheShare = (1ULL << 59) | (1ULL << 10);       /* BASER/CBASER */
const u64 RedistCacheShare = (0x7ULL << 7) | (1ULL << 10);     /* PROP/PENDBASER */

const u8 LpiPriority = 0xA0;

u32 Read32(ulong a) { return *reinterpret_cast<volatile u32*>(a); }
void Write32(ulong a, u32 v) { *reinterpret_cast<volatile u32*>(a) = v; }
u64 Read64(ulong a) { return *reinterpret_cast<volatile u64*>(a); }
void Write64(ulong a, u64 v) { *reinterpret_cast<volatile u64*>(a) = v; }

/* Allocate a zeroed, physically-contiguous region whose physical base is
   aligned to `align` (>= 64KiB for PENDBASER/BASER). AllocMapPages maps
   the pages contiguously in VA too, so the aligned VA is base_va +
   (aligned_phys - base_phys). Never freed (leaks the alignment slack). */
ulong AllocAligned(ulong bytes, ulong align, ulong* phys)
{
    ulong total = bytes + align;
    ulong pages = (total + Const::PageSize - 1) / Const::PageSize;
    ulong basePhys;
    void* baseVa = Mm::AllocMapPages(pages, &basePhys);
    if (!baseVa)
        return 0;

    ulong alignedPhys = (basePhys + (align - 1)) & ~(align - 1);
    ulong offset = alignedPhys - basePhys;
    ulong alignedVa = (ulong)baseVa + offset;

    Stdlib::MemSet((void*)alignedVa, 0, bytes);
    *phys = alignedPhys;
    return alignedVa;
}

ulong AllocZeroPages(ulong bytes, ulong* phys)
{
    return AllocAligned(bytes, 0x10000, phys);
}

}

bool Its::Setup(ulong itsPhys, ulong gicrBase, ulong gicrSize)
{
    ItsBase = Mm::PageTable::GetInstance().MapMmioRegion(itsPhys, 0x20000);
    if (ItsBase == 0)
        return false;
    ItsPhys = itsPhys;

    u64 typer = Read64(ItsBase + GitsTyper);
    EventIdBits = ((typer >> 8) & 0x1F) + 1;
    DeviceIdBits = ((typer >> 13) & 0x1F) + 1;
    IttEntrySize = ((typer >> 4) & 0xF) + 1;
    Pta = (typer >> 19) & 1;

    Trace(0, "Its: typer 0x%p eventbits %u devbits %u itt-entsz %u pta %u",
        typer, (ulong)EventIdBits, (ulong)DeviceIdBits, (ulong)IttEntrySize, (ulong)Pta);

    /* Config table covers all LPIs from 8192 up; size it to what we use.
       IDbits in PROPBASER counts total interrupt-id bits (incl. SPIs), so
       we cover LPI 8192 + MaxLpis; round the config table to a page. */
    ulong lpiConfigPhys;
    LpiConfigVa = AllocZeroPages(LpiIntIdBase + MaxLpis, &lpiConfigPhys);
    if (LpiConfigVa == 0)
        return false;
    /* Every used LPI: priority + not-enabled yet (enabled at MapEvent) */
    for (u32 i = 0; i < MaxLpis; i++)
        *reinterpret_cast<volatile u8*>(LpiConfigVa + i) = LpiPriority;
    asm volatile("dsb sy" ::: "memory");

    (void)lpiConfigPhys; /* PROPBASER phys is re-derived in EnableLpisOnRedist */
    if (!EnableLpisOnRedist(gicrBase, gicrSize))
        return false;

    if (!ProvisionTables())
        return false;
    if (!SetupCommandQueue())
        return false;

    Write32(ItsBase + GitsCtlr, GitsCtlrEnabled);

    /* Map the boot collection to this CPU's redistributor */
    BootIcid = 0;
    CmdMapc(BootIcid, BootRdBase, true);
    CmdSync(BootRdBase);
    WaitCommands();

    Ready = true;
    Trace(0, "Its: ready");
    return true;
}

bool Its::EnableLpisOnRedist(ulong gicrBase, ulong gicrSize)
{
    /* Find this CPU's redistributor (Aff match), like the GIC driver.
       For the BSP under QEMU virt this is the first frame. */
    ulong mpidr;
    asm volatile("mrs %0, mpidr_el1" : "=r"(mpidr));
    ulong aff = mpidr & 0xFFFFFF;

    /* gicrBase is physical; the redistributor lives in the premapped
       device GiB (phys < 1GiB on QEMU virt), so the device VA is
       KernelSpaceBase + phys — the same mapping the GIC driver uses. */
    ulong gicrVa = Mm::MemoryMap::KernelSpaceBase + gicrBase;

    ulong rd = 0;
    for (ulong off = 0; off < gicrSize; off += 0x20000)
    {
        ulong base = gicrVa + off;
        u64 typer = Read64(base + GicrTyper);
        if (((typer >> 32) & 0xFFFFFF) == aff)
        {
            rd = base;
            break;
        }
        if (typer & (1 << 4)) /* Last */
            break;
    }
    if (rd == 0)
        return false;

    /* LPI pending table: 1 bit per LPI id, covering 8192+MaxLpis; PENDBASER
       requires 64KiB alignment (the AllocAligned default). */
    ulong pendBytes = 0x10000;
    ulong pendPhys;
    if (AllocZeroPages(pendBytes, &pendPhys) == 0)
        return false;

    /* PROPBASER: config table phys + IDbits (total interrupt-id bits - 1).
       14 bits covers LPI ids up to 16383. */
    const u32 idBits = 14;
    ulong propPhys = Mm::PageTable::GetInstance().VirtToPhys(LpiConfigVa);

    u64 propbaser = (propPhys & 0x0000FFFFFFFFF000ULL) | RedistCacheShare |
                    (idBits - 1);
    u64 pendbaser = (pendPhys & 0x0000FFFFFFFF0000ULL) | RedistCacheShare;

    Write64(rd + GicrPropbaser, propbaser);
    Write64(rd + GicrPendbaser, pendbaser);
    asm volatile("dsb sy" ::: "memory");

    u32 ctlr = Read32(rd + GicrCtlr);
    Write32(rd + GicrCtlr, ctlr | GicrCtlrEnableLpis);
    asm volatile("dsb sy" ::: "memory");

    /* RDbase for MAPC/SYNC: PTA=1 -> RD_base phys >> 16; PTA=0 -> processor
       number (0 for the boot CPU on QEMU virt). */
    if (Pta)
    {
        ulong rdPhys = Mm::PageTable::GetInstance().VirtToPhys(rd);
        BootRdBase = rdPhys >> 16;
    }
    else
    {
        BootRdBase = 0;
    }

    Trace(0, "Its: LPIs enabled on rd 0x%p propbaser 0x%p", rd, propbaser);
    return true;
}

bool Its::ProvisionTables()
{
    /* Walk GITS_BASER[0..7]; provision Device (type 1) and Collection
       (type 4) tables. Flat (single-level) tables sized generously. */
    for (int i = 0; i < 8; i++)
    {
        ulong reg = ItsBase + GitsBaser0 + i * 8;
        u64 baser = Read64(reg);
        u32 type = (baser >> 56) & 7;
        if (type != 1 && type != 4)
            continue;

        u32 entrySize = ((baser >> 48) & 0x1F) + 1;

        /* Page size: force 64KiB (bits[9:10] = 0b10) for simplicity */
        u64 pageSizeSel = 2ULL << 8;
        ulong pageBytes = 0x10000;

        /* Entries: the device table is indexed by the PCI RID (deviceId);
           for QEMU virt bus 0 that fits in ~256 entries. Cap it well under
           the page allocator's 128-contiguous-page limit (a 64KiB-page
           table of a few pages). */
        ulong maxEntries = (type == 1) ? 4096 : 256;
        ulong idEntries = (type == 1) ? (1UL << (DeviceIdBits > 12 ? 12 : DeviceIdBits)) : 256;
        ulong entries = idEntries < maxEntries ? idEntries : maxEntries;
        ulong tableBytes = entries * entrySize;
        ulong pages = (tableBytes + pageBytes - 1) / pageBytes;
        if (pages == 0)
            pages = 1;

        ulong phys;
        ulong va = AllocZeroPages(pages * pageBytes, &phys);
        if (va == 0)
            return false;

        u64 val = (1ULL << 63) |            /* Valid */
                  ((u64)type << 56) |
                  ((u64)(entrySize - 1) << 48) |
                  BaserCacheShare |
                  pageSizeSel |
                  (phys & 0x0000FFFFFFFFF000ULL) |
                  (u64)(pages - 1);

        Write64(reg, val);
        u64 readback = Read64(reg);
        Trace(0, "Its: baser[%d] type %u entsz %u pages %u -> 0x%p",
            i, type, entrySize, pages, readback);
    }
    return true;
}

bool Its::SetupCommandQueue()
{
    CmdQueueBytes = 0x10000; /* 64KiB = 2048 commands */
    CmdQueueVa = AllocZeroPages(CmdQueueBytes, &CmdQueuePhys);
    if (CmdQueueVa == 0)
        return false;

    u64 cbaser = (CmdQueuePhys & 0x0000FFFFFFFFF000ULL) |
                 BaserCacheShare |
                 (1ULL << 63) |               /* Valid */
                 (u64)((CmdQueueBytes / 0x1000) - 1);
    Write64(ItsBase + GitsCbaser, cbaser);
    Write64(ItsBase + GitsCwriter, 0);
    CmdWriteOff = 0;
    return true;
}

void Its::PostCommand(const u64 cmd[4])
{
    volatile u64* slot = reinterpret_cast<volatile u64*>(CmdQueueVa + CmdWriteOff);
    slot[0] = cmd[0];
    slot[1] = cmd[1];
    slot[2] = cmd[2];
    slot[3] = cmd[3];
    asm volatile("dsb sy" ::: "memory");

    CmdWriteOff = (CmdWriteOff + 32) % CmdQueueBytes;
    Write64(ItsBase + GitsCwriter, CmdWriteOff);
}

void Its::WaitCommands()
{
    /* Command processed when CREADR catches up to CWRITER */
    for (ulong spin = 0; spin < 1000000; spin++)
    {
        if ((Read64(ItsBase + GitsCreadr) & ~0x1FULL) == CmdWriteOff)
            return;
        asm volatile("yield");
    }
    Trace(0, "Its: command queue stall (creadr 0x%p cwriter 0x%p)",
        Read64(ItsBase + GitsCreadr), CmdWriteOff);
}

void Its::CmdMapd(u32 deviceId, u64 ittPhys, u32 sizeBits, bool valid)
{
    u64 cmd[4] = {0, 0, 0, 0};
    cmd[0] = CmdMapdOp | ((u64)deviceId << 32);
    cmd[1] = (sizeBits - 1) & 0x1F;
    cmd[2] = (ittPhys & 0x0000FFFFFFFFFF00ULL) | (valid ? (1ULL << 63) : 0);
    PostCommand(cmd);
}

void Its::CmdMapc(u16 icid, u64 rdBase, bool valid)
{
    u64 cmd[4] = {0, 0, 0, 0};
    cmd[0] = CmdMapcOp;
    cmd[2] = ((u64)icid) | (rdBase << 16) | (valid ? (1ULL << 63) : 0);
    PostCommand(cmd);
}

void Its::CmdMapti(u32 deviceId, u32 eventId, u32 lpiIntId, u16 icid)
{
    u64 cmd[4] = {0, 0, 0, 0};
    cmd[0] = CmdMaptiOp | ((u64)deviceId << 32);
    cmd[1] = (u64)eventId | ((u64)lpiIntId << 32);
    cmd[2] = (u64)icid;
    PostCommand(cmd);
}

void Its::CmdInv(u32 deviceId, u32 eventId)
{
    u64 cmd[4] = {0, 0, 0, 0};
    cmd[0] = CmdInvOp | ((u64)deviceId << 32);
    cmd[1] = (u64)eventId;
    PostCommand(cmd);
}

void Its::CmdSync(u64 rdBase)
{
    u64 cmd[4] = {0, 0, 0, 0};
    cmd[0] = CmdSyncOp;
    cmd[2] = rdBase << 16;
    PostCommand(cmd);
}

bool Its::MapDevice(u32 deviceId, u32 numEvents)
{
    if (!Ready)
        return false;

    for (u32 i = 0; i < MaxDevices; i++)
        if (Devices[i].Used && Devices[i].DeviceId == deviceId)
            return true; /* already mapped */

    u32 free = MaxDevices;
    for (u32 i = 0; i < MaxDevices; i++)
        if (!Devices[i].Used) { free = i; break; }
    if (free == MaxDevices)
        return false;

    /* ITT: numEvents entries, IttEntrySize bytes each, 256-byte aligned */
    u32 eventBits = 1;
    while ((1u << eventBits) < numEvents)
        eventBits++;
    if (eventBits < 1)
        eventBits = 1;

    ulong ittBytes = ((ulong)1 << eventBits) * IttEntrySize;
    if (ittBytes < 256)
        ittBytes = 256;
    ulong ittPhys;
    ulong ittVa = AllocZeroPages(ittBytes, &ittPhys);
    if (ittVa == 0)
        return false;

    CmdMapd(deviceId, ittPhys, eventBits, true);
    CmdSync(BootRdBase);
    WaitCommands();

    Devices[free].DeviceId = deviceId;
    Devices[free].Used = true;
    Trace(0, "Its: mapped device 0x%p itt 0x%p eventbits %u",
        (ulong)deviceId, ittPhys, eventBits);
    return true;
}

u32 Its::AllocLpi()
{
    if (NextLpi >= MaxLpis)
        return 0;
    return LpiIntIdBase + NextLpi++;
}

u32 Its::MapEvent(u32 deviceId, u32 eventId, InterruptHandler& handler,
    u64& msiAddr)
{
    if (!Ready)
        return 0;

    u32 lpi = AllocLpi();
    if (lpi == 0)
    {
        Trace(0, "Its: out of LPIs");
        return 0;
    }

    u32 slot = lpi - LpiIntIdBase;
    LpiHandlers[slot].Handler = &handler;

    /* Enable this LPI in the config table + invalidate the cached copy */
    *reinterpret_cast<volatile u8*>(LpiConfigVa + slot) = LpiPriority | 1;
    asm volatile("dsb sy" ::: "memory");

    CmdMapti(deviceId, eventId, lpi, BootIcid);
    CmdInv(deviceId, eventId);
    CmdSync(BootRdBase);
    WaitCommands();

    /* The device DMA-writes the eventId to the ITS translate register; hand
       back its PHYSICAL address. (VirtToPhys can't resolve ItsBase: it lives
       in the 1GiB device block, not 4K PTEs, so use the stored phys base.) */
    msiAddr = ItsPhys + GitsTranslater;

    handler.OnInterruptRegister((u8)0, (u8)0);

    Trace(0, "Its: event dev 0x%p ev %u -> lpi %u addr 0x%p",
        (ulong)deviceId, (ulong)eventId, (ulong)lpi, msiAddr);
    return eventId; /* MSI data = eventId */
}

void Its::HandleLpi(u32 intId)
{
    u32 slot = intId - LpiIntIdBase;
    if (slot >= MaxLpis || !LpiHandlers[slot].Handler)
        return;
    LpiHandlers[slot].Handler->OnInterrupt(nullptr);
}

}
