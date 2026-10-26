#include "its.h"

#include <kernel/interrupt.h>
#include <kernel/trace.h>
#include <kernel/panic.h>
#include <hal/context.h>
#include <mm/new.h>
#include <mm/page_table.h>
#include <mm/memory_map.h>
#include <lib/stdlib.h>

namespace kernel
{

namespace
{

/* ITS register offsets */
const ulong GITS_CTLR = 0x0000;
const ulong GITS_TYPER = 0x0008;
const ulong GITS_CBASER = 0x0080;
const ulong GITS_CWRITER = 0x0088;
const ulong GITS_CREADR = 0x0090;
const ulong GITS_BASER0 = 0x0100;
const ulong GITS_TRANSLATER = 0x10040;

/* Redistributor LPI registers (RD_base) */
const ulong GICR_CTLR = 0x0000;
const ulong GICR_TYPER = 0x0008;
const ulong GICR_PROPBASER = 0x0070;
const ulong GICR_PENDBASER = 0x0078;

const u32 GITS_CTLR_ENABLED = 1;
const u32 GICR_CTLR_ENABLE_LPIS = 1;

/* Command opcodes */
const u64 CMD_SYNC_OP = 0x05;
const u64 CMD_MAPD_OP = 0x08;
const u64 CMD_MAPC_OP = 0x09;
const u64 CMD_MAPTI_OP = 0x0A;
const u64 CMD_INV_OP = 0x0C;

/* Cacheability/shareability attributes. The field *positions* differ between
   register families:
   - GITS_BASER / GITS_CBASER: InnerCache[61:59], Shareability[11:10]
   - GICR_PROPBASER / GICR_PENDBASER: InnerCache[9:7], Shareability[11:10]
   Both set Normal Inner WB write-allocate + Inner shareable. Mixing a
   shareable + non-cacheable encoding (the bug of using the BASER layout for
   PROPBASER) wedges the redistributor. */
const u64 BASER_CACHE_SHARE = (0x7ULL << 59) | (1ULL << 10);     /* BASER/CBASER */
const u64 REDIST_CACHE_SHARE = (0x7ULL << 7) | (1ULL << 10);     /* PROP/PENDBASER */

const u8 LPI_PRIORITY = 0xA0;

u32 read32(ulong a) { return *reinterpret_cast<volatile u32*>(a); }
void write32(ulong a, u32 v) { *reinterpret_cast<volatile u32*>(a) = v; }
u64 read64(ulong a) { return *reinterpret_cast<volatile u64*>(a); }
void write64(ulong a, u64 v) { *reinterpret_cast<volatile u64*>(a) = v; }

/* Allocate a zeroed, physically-contiguous region whose physical base is
   aligned to `align` (>= 64KiB for PENDBASER/BASER). AllocMapPages maps
   the pages contiguously in VA too, so the aligned VA is base_va +
   (aligned_phys - base_phys). Never freed (leaks the alignment slack). */
ulong alloc_aligned(ulong bytes, ulong align, ulong* phys)
{
    ulong total = bytes + align;
    ulong pages = (total + consts::PAGE_SIZE - 1) / consts::PAGE_SIZE;
    ulong base_phys;
    void* base_va = mm::alloc_map_pages(pages, &base_phys);
    if (!base_va)
        return 0;

    ulong aligned_phys = (base_phys + (align - 1)) & ~(align - 1);
    ulong offset = aligned_phys - base_phys;
    ulong aligned_va = (ulong)base_va + offset;

    stdlib::mem_set((void*)aligned_va, 0, bytes);
    *phys = aligned_phys;
    return aligned_va;
}

ulong alloc_zero_pages(ulong bytes, ulong* phys)
{
    return alloc_aligned(bytes, 0x10000, phys);
}

}

bool Its::setup(ulong its_phys, ulong gicr_base, ulong gicr_size)
{
    its_base_ = mm::PageTable::get_instance().map_mmio_region(its_phys, 0x20000);
    if (its_base_ == 0)
        return false;
    its_phys_ = its_phys;

    u64 typer = read64(its_base_ + GITS_TYPER);
    event_id_bits_ = ((typer >> 8) & 0x1F) + 1;
    device_id_bits_ = ((typer >> 13) & 0x1F) + 1;
    itt_entry_size_ = ((typer >> 4) & 0xF) + 1;
    pta_ = (typer >> 19) & 1;

    trace(0, "Its: typer 0x%lX eventbits %lu devbits %lu itt-entsz %lu pta %lu",
        typer, (ulong)event_id_bits_, (ulong)device_id_bits_, (ulong)itt_entry_size_, (ulong)pta_);

    /* Config table covers all LPIs from 8192 up; size it to what we use.
       IDbits in PROPBASER counts total interrupt-id bits (incl. SPIs), so
       we cover LPI 8192 + MaxLpis; round the config table to a page. */
    ulong lpi_config_phys;
    lpi_config_va_ = alloc_zero_pages(LPI_INT_ID_BASE + MAX_LPIS, &lpi_config_phys);
    if (lpi_config_va_ == 0)
        return false;
    /* Every used LPI: priority + not-enabled yet (enabled at MapEvent) */
    for (u32 i = 0; i < MAX_LPIS; i++)
        *reinterpret_cast<volatile u8*>(lpi_config_va_ + i) = LPI_PRIORITY;
    asm volatile("dsb sy" ::: "memory");

    (void)lpi_config_phys; /* PROPBASER phys is re-derived in EnableLpisOnRedist */
    if (!enable_lpis_on_redist(gicr_base, gicr_size))
        return false;

    if (!provision_tables())
        return false;
    if (!setup_command_queue())
        return false;

    write32(its_base_ + GITS_CTLR, GITS_CTLR_ENABLED);

    /* Map the boot collection to this CPU's redistributor */
    boot_icid_ = 0;
    cmd_mapc(boot_icid_, boot_rd_base_, true);
    cmd_sync(boot_rd_base_);
    if (!wait_commands())
        return false;

    ready_ = true;
    trace(0, "Its: ready");
    return true;
}

bool Its::enable_lpis_on_redist(ulong gicr_base, ulong gicr_size)
{
    /* Find this CPU's redistributor (Aff match), like the GIC driver.
       For the BSP under QEMU virt this is the first frame. */
    ulong mpidr;
    asm volatile("mrs %0, mpidr_el1" : "=r"(mpidr));
    ulong aff = mpidr & 0xFFFFFF;

    /* gicrBase is physical; the redistributor lives in the premapped
       device GiB (phys < 1GiB on QEMU virt), so the device VA is
       KernelSpaceBase + phys — the same mapping the GIC driver uses. */
    ulong gicr_va = mm::MemoryMap::KERNEL_SPACE_BASE + gicr_base;

    ulong rd = 0;
    for (ulong off = 0; off < gicr_size; off += 0x20000)
    {
        ulong base = gicr_va + off;
        u64 typer = read64(base + GICR_TYPER);
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
    ulong pend_bytes = 0x10000;
    ulong pend_phys;
    if (alloc_zero_pages(pend_bytes, &pend_phys) == 0)
        return false;

    /* PROPBASER: config table phys + IDbits (total interrupt-id bits - 1).
       14 bits covers LPI ids up to 16383. */
    const u32 id_bits = 14;
    ulong prop_phys = mm::PageTable::get_instance().virt_to_phys(lpi_config_va_);

    u64 propbaser = (prop_phys & 0x0000FFFFFFFFF000ULL) | REDIST_CACHE_SHARE |
                    (id_bits - 1);
    u64 pendbaser = (pend_phys & 0x0000FFFFFFFF0000ULL) | REDIST_CACHE_SHARE;

    write64(rd + GICR_PROPBASER, propbaser);
    write64(rd + GICR_PENDBASER, pendbaser);
    asm volatile("dsb sy" ::: "memory");

    u32 ctlr = read32(rd + GICR_CTLR);
    write32(rd + GICR_CTLR, ctlr | GICR_CTLR_ENABLE_LPIS);
    asm volatile("dsb sy" ::: "memory");

    /* RDbase for MAPC/SYNC: PTA=1 -> RD_base phys >> 16; PTA=0 -> processor
       number (0 for the boot CPU on QEMU virt). The phys is derived
       arithmetically: rd lives in the premapped device GiB, which
       VirtToPhys cannot walk (same reason ItsPhys is stored explicitly). */
    if (pta_)
    {
        ulong rd_phys = gicr_base + (rd - gicr_va);
        boot_rd_base_ = rd_phys >> 16;
    }
    else
    {
        boot_rd_base_ = 0;
    }

    trace(0, "Its: LPIs enabled on rd 0x%lX propbaser 0x%lX", rd, propbaser);
    return true;
}

bool Its::provision_tables()
{
    /* Walk GITS_BASER[0..7]; provision Device (type 1) and Collection
       (type 4) tables. Flat (single-level) tables sized generously. */
    for (int i = 0; i < 8; i++)
    {
        ulong reg = its_base_ + GITS_BASER0 + i * 8;
        u64 baser = read64(reg);
        u32 type = (baser >> 56) & 7;
        if (type != 1 && type != 4)
            continue;

        u32 entry_size = ((baser >> 48) & 0x1F) + 1;

        /* Page size: force 64KiB (bits[9:10] = 0b10) for simplicity */
        u64 page_size_sel = 2ULL << 8;
        ulong page_bytes = 0x10000;

        /* Entries: the device table is indexed by the PCI RID (deviceId);
           for QEMU virt bus 0 that fits in ~256 entries. Cap it well under
           the page allocator's 128-contiguous-page limit (a 64KiB-page
           table of a few pages). */
        ulong max_entries = (type == 1) ? 4096 : 256;
        ulong id_entries = (type == 1) ? (1UL << (device_id_bits_ > 12 ? 12 : device_id_bits_)) : 256;
        ulong entries = id_entries < max_entries ? id_entries : max_entries;
        ulong table_bytes = entries * entry_size;
        ulong pages = (table_bytes + page_bytes - 1) / page_bytes;
        if (pages == 0)
            pages = 1;

        ulong phys;
        ulong va = alloc_zero_pages(pages * page_bytes, &phys);
        if (va == 0)
            return false;

        u64 val = (1ULL << 63) |            /* Valid */
                  ((u64)type << 56) |
                  ((u64)(entry_size - 1) << 48) |
                  BASER_CACHE_SHARE |
                  page_size_sel |
                  (phys & 0x0000FFFFFFFFF000ULL) |
                  (u64)(pages - 1);

        write64(reg, val);
        u64 readback = read64(reg);
        trace(0, "Its: baser[%lu] type %lu entsz %lu pages %lu -> 0x%lX",
            (ulong)i, (ulong)type, (ulong)entry_size, pages, readback);
    }
    return true;
}

bool Its::setup_command_queue()
{
    cmd_queue_bytes_ = 0x10000; /* 64KiB = 2048 commands */
    cmd_queue_va_ = alloc_zero_pages(cmd_queue_bytes_, &cmd_queue_phys_);
    if (cmd_queue_va_ == 0)
        return false;

    u64 cbaser = (cmd_queue_phys_ & 0x0000FFFFFFFFF000ULL) |
                 BASER_CACHE_SHARE |
                 (1ULL << 63) |               /* Valid */
                 (u64)((cmd_queue_bytes_ / 0x1000) - 1);
    write64(its_base_ + GITS_CBASER, cbaser);
    write64(its_base_ + GITS_CWRITER, 0);
    cmd_write_off_ = 0;
    return true;
}

void Its::post_command(const u64 cmd[4])
{
    volatile u64* slot = reinterpret_cast<volatile u64*>(cmd_queue_va_ + cmd_write_off_);
    slot[0] = cmd[0];
    slot[1] = cmd[1];
    slot[2] = cmd[2];
    slot[3] = cmd[3];
    asm volatile("dsb sy" ::: "memory");

    cmd_write_off_ = (cmd_write_off_ + 32) % cmd_queue_bytes_;
    write64(its_base_ + GITS_CWRITER, cmd_write_off_);
}

bool Its::wait_commands()
{
    /* Command processed when CREADR catches up to CWRITER */
    for (ulong spin = 0; spin < 1000000; spin++)
    {
        if ((read64(its_base_ + GITS_CREADR) & ~0x1FULL) == cmd_write_off_)
            return true;
        asm volatile("yield");
    }
    trace(0, "Its: command queue stall (creadr 0x%lX cwriter 0x%lX)",
        read64(its_base_ + GITS_CREADR), cmd_write_off_);
    return false;
}

void Its::cmd_mapd(u32 device_id, u64 itt_phys, u32 size_bits, bool valid)
{
    u64 cmd[4] = {0, 0, 0, 0};
    cmd[0] = CMD_MAPD_OP | ((u64)device_id << 32);
    cmd[1] = (size_bits - 1) & 0x1F;
    cmd[2] = (itt_phys & 0x0000FFFFFFFFFF00ULL) | (valid ? (1ULL << 63) : 0);
    post_command(cmd);
}

void Its::cmd_mapc(u16 icid, u64 rd_base, bool valid)
{
    u64 cmd[4] = {0, 0, 0, 0};
    cmd[0] = CMD_MAPC_OP;
    cmd[2] = ((u64)icid) | (rd_base << 16) | (valid ? (1ULL << 63) : 0);
    post_command(cmd);
}

void Its::cmd_mapti(u32 device_id, u32 event_id, u32 lpi_int_id, u16 icid)
{
    u64 cmd[4] = {0, 0, 0, 0};
    cmd[0] = CMD_MAPTI_OP | ((u64)device_id << 32);
    cmd[1] = (u64)event_id | ((u64)lpi_int_id << 32);
    cmd[2] = (u64)icid;
    post_command(cmd);
}

void Its::cmd_inv(u32 device_id, u32 event_id)
{
    u64 cmd[4] = {0, 0, 0, 0};
    cmd[0] = CMD_INV_OP | ((u64)device_id << 32);
    cmd[1] = (u64)event_id;
    post_command(cmd);
}

void Its::cmd_sync(u64 rd_base)
{
    u64 cmd[4] = {0, 0, 0, 0};
    cmd[0] = CMD_SYNC_OP;
    cmd[2] = rd_base << 16;
    post_command(cmd);
}

bool Its::map_device(u32 device_id, u32 num_events)
{
    if (!ready_)
        return false;

    for (u32 i = 0; i < MAX_DEVICES; i++)
        if (devices_[i].used && devices_[i].device_id == device_id)
            return true; /* already mapped */

    u32 free = MAX_DEVICES;
    for (u32 i = 0; i < MAX_DEVICES; i++)
        if (!devices_[i].used) { free = i; break; }
    if (free == MAX_DEVICES)
        return false;

    /* ITT: numEvents entries, IttEntrySize bytes each, 256-byte aligned */
    u32 event_bits = 1;
    while ((1u << event_bits) < num_events)
        event_bits++;
    if (event_bits < 1)
        event_bits = 1;

    ulong itt_bytes = ((ulong)1 << event_bits) * itt_entry_size_;
    if (itt_bytes < 256)
        itt_bytes = 256;
    ulong itt_phys;
    ulong itt_va = alloc_zero_pages(itt_bytes, &itt_phys);
    if (itt_va == 0)
        return false;

    cmd_mapd(device_id, itt_phys, event_bits, true);
    cmd_sync(boot_rd_base_);
    if (!wait_commands())
        return false; /* the ITT allocation is leaked by design (never freed) */

    devices_[free].device_id = device_id;
    devices_[free].used = true;
    trace(0, "Its: mapped device 0x%lX itt 0x%lX eventbits %u",
        (ulong)device_id, itt_phys, event_bits);
    return true;
}

u32 Its::alloc_lpi()
{
    if (next_lpi_ >= MAX_LPIS)
        return 0;
    return LPI_INT_ID_BASE + next_lpi_++;
}

u32 Its::map_event(u32 device_id, u32 event_id, InterruptHandler& handler,
    u64& msi_addr)
{
    if (!ready_)
        return 0;

    u32 lpi = alloc_lpi();
    if (lpi == 0)
    {
        trace(0, "Its: out of LPIs");
        return 0;
    }

    u32 slot = lpi - LPI_INT_ID_BASE;
    lpi_handlers_[slot].handler = &handler;

    /* Enable this LPI in the config table + invalidate the cached copy */
    *reinterpret_cast<volatile u8*>(lpi_config_va_ + slot) = LPI_PRIORITY | 1;
    asm volatile("dsb sy" ::: "memory");

    cmd_mapti(device_id, event_id, lpi, boot_icid_);
    cmd_inv(device_id, event_id);
    cmd_sync(boot_rd_base_);
    if (!wait_commands())
    {
        /* Don't hand the driver a dead interrupt: report failure (the
           LPI id and config-table slot stay burned, which is fine) */
        lpi_handlers_[slot].handler = nullptr;
        return 0;
    }

    /* The device DMA-writes the eventId to the ITS translate register; hand
       back its PHYSICAL address. (VirtToPhys can't resolve ItsBase: it lives
       in the 1GiB device block, not 4K PTEs, so use the stored phys base.) */
    msi_addr = its_phys_ + GITS_TRANSLATER;

    handler.on_interrupt_register((u8)0, (u8)0);

    trace(0, "Its: event dev 0x%lX ev %lu -> lpi %lu addr 0x%lX",
        (ulong)device_id, (ulong)event_id, (ulong)lpi, msi_addr);
    return event_id; /* MSI data = eventId */
}

void Its::handle_lpi(u32 int_id)
{
    u32 slot = int_id - LPI_INT_ID_BASE;
    if (slot >= MAX_LPIS || !lpi_handlers_[slot].handler)
        return;
    lpi_handlers_[slot].handler->on_interrupt(nullptr);
}

}
