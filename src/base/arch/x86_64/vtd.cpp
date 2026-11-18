#include "vtd.h"
#include "idt.h"
#include "idt_descriptor.h"

#include <drivers/pci.h>
#include <drivers/mmio.h>
#include <hal/barrier.h>
#include <hal/irqchip.h>
#include <hal/context.h>
#include <include/const.h>
#include <kernel/klog.h>
#include <kernel/parameters.h>
#include <kernel/time.h>
#include <mm/memory_map.h>
#include <mm/new.h>
#include <mm/page_table.h>

extern "C" void vtd_fault_interrupt_stub();

namespace kernel
{

namespace
{

/* The registers (11.4), at their offsets from a unit's base */
const ulong REG_VER = 0x00;
const ulong REG_CAP = 0x08;
const ulong REG_ECAP = 0x10;
const ulong REG_GCMD = 0x18;
const ulong REG_GSTS = 0x1C;
const ulong REG_RTADDR = 0x20;
const ulong REG_FSTS = 0x34;
const ulong REG_FECTL = 0x38;
const ulong REG_FEDATA = 0x3C;
const ulong REG_FEADDR = 0x40;
const ulong REG_FEUADDR = 0x44;
const ulong REG_PMEN = 0x64;
const ulong REG_IQH = 0x80;
const ulong REG_IQT = 0x88;
const ulong REG_IQA = 0x90;
/* The last of the fixed registers this reads, and its width: the IRTA */
const ulong REGS_FIXED_END = 0xC0;

/* GCMD and GSTS (11.4.4, 11.4.5): a bit a function, at the same place in
   both. A state -- translation, the advanced fault log, queued
   invalidation, interrupt remapping, compatibility-format interrupts -- is
   held for as long as GCMD has its bit; a command -- set the root table's
   pointer, the interrupt remapping table's, flush the write buffer -- is
   done once for each write that has it, its status bit set when it is. */
const u32 GS_TE = 1u << 31;
const u32 GS_SRTP = 1u << 30;
const u32 GS_EAFL = 1u << 28;
const u32 GS_WBF = 1u << 27;
const u32 GS_QIE = 1u << 26;
const u32 GS_IRE = 1u << 25;
const u32 GS_CFI = 1u << 23;
const u32 GS_STATES = GS_TE | GS_EAFL | GS_QIE | GS_IRE | GS_CFI;

/* CAP (11.4.2) */
u64 cap_domains(u64 cap) { return 1ULL << (4 + 2 * (cap & 0x7)); }
bool cap_rwbf(u64 cap) { return (cap & (1ULL << 4)) != 0; }
bool cap_caching_mode(u64 cap) { return (cap & (1ULL << 7)) != 0; }
u64 cap_sagaw(u64 cap) { return (cap >> 8) & 0x1F; }
u64 cap_mgaw(u64 cap) { return ((cap >> 16) & 0x3F) + 1; }
ulong cap_fault_offset(u64 cap) { return static_cast<ulong>((cap >> 24) & 0x3FF) * 16; }
u64 cap_superpages(u64 cap) { return (cap >> 34) & 0xF; }
ulong cap_fault_count(u64 cap) { return static_cast<ulong>((cap >> 40) & 0xFF) + 1; }

/* ECAP (11.4.3) */
bool ecap_coherent(u64 ecap) { return (ecap & (1ULL << 0)) != 0; }
bool ecap_queued(u64 ecap) { return (ecap & (1ULL << 1)) != 0; }
bool ecap_intr_remap(u64 ecap) { return (ecap & (1ULL << 3)) != 0; }
bool ecap_x2apic(u64 ecap) { return (ecap & (1ULL << 4)) != 0; }
bool ecap_pass_through(u64 ecap) { return (ecap & (1ULL << 6)) != 0; }
bool ecap_snoop_control(u64 ecap) { return (ecap & (1ULL << 7)) != 0; }
ulong ecap_iotlb_offset(u64 ecap) { return static_cast<ulong>((ecap >> 8) & 0x3FF) * 16; }

/* SAGAW's bits, and the AW a context entry names each by (9.3) */
const u64 SAGAW_39 = 1 << 1;
const u64 SAGAW_48 = 1 << 2;
const u64 SAGAW_57 = 1 << 3;
const u8 AW_39 = 1;
const u8 AW_48 = 2;
const u8 AW_57 = 3;

/* FSTS (11.4.6): its write-one-to-clear bits, and where the fault
   recording register to look at first is said */
const u32 FSTS_PFO = 1u << 0;
const u32 FSTS_PPF = 1u << 1;
const u32 FSTS_IQE = 1u << 4;
const u32 FSTS_ICE = 1u << 5;
const u32 FSTS_ITE = 1u << 6;
const u32 FSTS_CLEARED = FSTS_PFO | FSTS_IQE | FSTS_ICE | FSTS_ITE;
const ulong FSTS_FRI_SHIFT = 8;
const u32 FSTS_FRI_MASK = 0xFF;

/* FECTL (11.4.7): the fault event's interrupt masked */
const u32 FECTL_IM = 1u << 31;

/* The fault event's message (11.4.8-10): the local APIC's address, the
   destination's id in bits 19:12, a fixed edge interrupt of the vector */
const u32 MSI_ADDRESS_BASE = 0xFEE00000;
const ulong MSI_DEST_SHIFT = 12;
const u32 MSI_DEST_LIMIT = 0xFF;

/* PMEN (11.4.13): protected memory regions on, and its status */
const u32 PMEN_EPM = 1u << 31;
const u32 PMEN_PRS = 1u << 0;

/* A fault recording register (11.4.14), 16 bytes from the unit's FRO: the
   faulting page's address in the low word; in the high word the
   requester's id (15:0), the reason (39:32), the type -- set for a read --
   (62) and F (63), written with a one to clear it */
const ulong FRCD_HIGH = 8;
const ulong FRCD_HIGH_UPPER = 12;
const u64 FRCD_F = 1ULL << 63;
const u32 FRCD_F_UPPER = 1u << 31;
const u64 FRCD_READ = 1ULL << 62;
const ulong FRCD_REASON_SHIFT = 32;
const u64 FRCD_SID_MASK = 0xFFFF;
const u64 FRCD_ADDRESS_MASK = ~0xFFFULL;

/* A root entry (9.1) and a context entry (9.3): 16 bytes, as two words */
const u64 ENTRY_PRESENT = 1;
const u64 CONTEXT_PASS_THROUGH = 2ULL << 2;
const ulong CONTEXT_DID_SHIFT = 8;
const size_t ENTRY_WORDS = 2;
/* The domain id the passed-through entries name: 0 is the one a unit in
   caching mode keeps for itself (6.2.2), which QEMU's is */
const u64 DID_PASS_THROUGH = 1;

/* Invalidation descriptors (6.5.2), 128 bits as two words: the context
   cache's, the IOTLB's -- globally, every domain, reads and writes drained
   -- and the wait whose status write says the ones before it are done */
const u64 INV_CONTEXT_GLOBAL = 0x1 | (1ULL << 4);
const u64 INV_IOTLB_GLOBAL = 0x2 | (1ULL << 4) | (1ULL << 6) | (1ULL << 7);
const u64 INV_WAIT = 0x5;
const u64 INV_WAIT_STATUS_WRITE = 1ULL << 5;
const u64 INV_WAIT_FENCE = 1ULL << 6;
const ulong INV_WAIT_DATA_SHIFT = 32;
const ulong QUEUE_DESCRIPTORS = consts::PAGE_SIZE / (ENTRY_WORDS * sizeof(u64));
/* IQT and IQH hold a descriptor's index from bit 4 (11.4.15, 11.4.16) */
const ulong QUEUE_INDEX_SHIFT = 4;

/* A cache line, as clflush takes them */
const ulong CACHE_LINE = 64;

/* The most pages of registers a unit is taken to have: 2^size, size from
   its DRHD (VT-d 3.0) */
const u8 MAX_REG_SIZE_LOG = 4;

/* Who is said: a requester id as bus:device.function */
u8 sid_bus(u16 sid) { return static_cast<u8>(sid >> 8); }
u8 sid_dev(u16 sid) { return static_cast<u8>((sid >> 3) & 0x1F); }
u8 sid_fn(u16 sid) { return static_cast<u8>(sid & 0x7); }
const u16 SID_DEVFN_MASK = 0xFF;
const ulong SID_BUS_SHIFT = 8;

/* A context entry naming a domain: untranslated requests through its
   second-level table, and nothing else (TT=00) */
const u64 CONTEXT_UNTRANSLATED = 0ULL << 2;
const u64 CONTEXT_DID_MASK = 0xFFFF;

/* Second-level entries (9.8): readable, writable, a leaf above the last
   level (2 MiB), DMA made to snoop (ECAP.SC), the address */
const u64 SL_READ = 1ULL << 0;
const u64 SL_WRITE = 1ULL << 1;
const u64 SL_PAGE_SIZE = 1ULL << 7;
const u64 SL_SNOOP = 1ULL << 11;
const u64 SL_ADDRESS = 0x000FFFFFFFFFF000ULL;
const ulong SHIFT_4K = 12;
const ulong SHIFT_2M = 21;
const ulong LEVEL_BITS = 9;
/* CAP.SLLPS's bit for 2 MiB leaves */
const u64 SLLPS_2M = 1;

/* One device's context cached, and one domain's IOTLB, dropped (6.5.2.1,
   6.5.2.2): G 11 and 10, the domain id from bit 16, the device's id from
   bit 32 */
u64 inv_context_device(u16 did, u16 sid)
{
    return 0x1 | (3ULL << 4) | (static_cast<u64>(did) << 16) | (static_cast<u64>(sid) << 32);
}

u64 inv_iotlb_domain(u16 did)
{
    return 0x2 | (2ULL << 4) | (1ULL << 6) | (1ULL << 7) | (static_cast<u64>(did) << 16);
}

}

Vtd::Vtd()
    : unit_count_(0)
    , last_bus_(0)
    , on_(false)
    , fault_apic_id_(0)
{
    for (size_t i = 0; i < stdlib::array_size(units_); i++)
        units_[i] = nullptr;
    for (size_t i = 0; i < stdlib::array_size(domains_); i++)
        domains_[i] = nullptr;
    for (size_t i = 0; i < MAX_SHELL_DOMAINS; i++)
    {
        shell_domains_[i].sid = 0;
        shell_domains_[i].handle = 0;
    }
}

Vtd::~Vtd()
{
}

u32 Vtd::read32(const Unit& unit, ulong offset) const
{
    return mmio_read32(unit.regs + offset);
}

u64 Vtd::read64(const Unit& unit, ulong offset) const
{
    return mmio_read64(unit.regs + offset);
}

void Vtd::write32(Unit& unit, ulong offset, u32 value)
{
    mmio_write32(unit.regs + offset, value);
}

void Vtd::write64(Unit& unit, ulong offset, u64 value)
{
    mmio_write64(unit.regs + offset, value);
}

bool Vtd::wait_register(Unit& unit, ulong offset, u32 mask, u32 value)
{
    for (ulong waited = 0; waited < WAIT_NS; waited += WAIT_STEP_NS)
    {
        if ((read32(unit, offset) & mask) == value)
            return true;
        busy_wait(WAIT_STEP_NS);
    }
    return (read32(unit, offset) & mask) == value;
}

bool Vtd::command(Unit& unit, u32 bit, bool on)
{
    const u32 states = read32(unit, REG_GSTS) & GS_STATES;
    write32(unit, REG_GCMD, on ? (states | bit) : (states & ~bit));
    if (wait_register(unit, REG_GSTS, bit, on ? bit : 0))
        return true;
    klog(0, "vtd: dmar%lu: GCMD 0x%lX not done in %lu ms, GSTS 0x%lX", (ulong)unit.index, (ulong)bit,
        WAIT_NS / (1000 * 1000), (ulong)read32(unit, REG_GSTS));
    return false;
}

bool Vtd::flush_write_buffer(Unit& unit)
{
    /* Its status is set by the command and cleared when the flush is done
       (11.4.5), the other way round from the root table's */
    const u32 states = read32(unit, REG_GSTS) & GS_STATES;
    write32(unit, REG_GCMD, states | GS_WBF);
    if (wait_register(unit, REG_GSTS, GS_WBF, 0))
        return true;
    klog(0, "vtd: dmar%lu: its write buffer not flushed in %lu ms", (ulong)unit.index, WAIT_NS / (1000 * 1000));
    return false;
}

void Vtd::flush(const Unit& unit, const void* va, ulong bytes) const
{
    if (ecap_coherent(unit.ecap) || bytes == 0)
        return;
    const ulong start = reinterpret_cast<ulong>(va) & ~(CACHE_LINE - 1);
    const ulong end = reinterpret_cast<ulong>(va) + bytes;
    asm volatile("mfence" ::: "memory");
    for (ulong line = start; line < end; line += CACHE_LINE)
        asm volatile("clflush (%0)" :: "r"(line) : "memory");
    asm volatile("mfence" ::: "memory");
}

bool Vtd::invalidate(Unit& unit, u64 lo, u64 hi)
{
    stdlib::AutoLock lock(unit.queue_lock);
    if (unit.broken)
    {
        klog(0, "vtd: dmar%lu: its invalidation queue stopped before: nothing more queued", (ulong)unit.index);
        return false;
    }

    const u32 seq = ++unit.status_seq;
    *unit.status = 0;

    const ulong first = unit.queue_tail;
    const u64 descriptors[2][ENTRY_WORDS] = {
        {lo, hi},
        {INV_WAIT | INV_WAIT_STATUS_WRITE | INV_WAIT_FENCE | (static_cast<u64>(seq) << INV_WAIT_DATA_SHIFT),
            unit.status_phys},
    };
    for (size_t i = 0; i < stdlib::array_size(descriptors); i++)
    {
        u64* slot = unit.queue + unit.queue_tail * ENTRY_WORDS;
        slot[0] = descriptors[i][0];
        slot[1] = descriptors[i][1];
        flush(unit, slot, ENTRY_WORDS * sizeof(u64));
        unit.queue_tail = (unit.queue_tail + 1) % QUEUE_DESCRIPTORS;
    }

    /* The descriptors in memory before the unit is told of them */
    hal::dma_wmb();
    write64(unit, REG_IQT, static_cast<u64>(unit.queue_tail) << QUEUE_INDEX_SHIFT);

    for (ulong waited = 0; waited <= WAIT_NS; waited += WAIT_STEP_NS)
    {
        if (*unit.status == seq)
        {
            hal::dma_rmb();
            return true;
        }
        const u32 fsts = read32(unit, REG_FSTS);
        if (fsts & (FSTS_IQE | FSTS_ICE | FSTS_ITE))
        {
            const u64 head = read64(unit, REG_IQH) >> QUEUE_INDEX_SHIFT;
            klog(0, "vtd: dmar%lu: the invalidation queue stopped at %lu (submitted from %lu): FSTS 0x%lX, "
                "descriptor 0x%lX 0x%lX", (ulong)unit.index, (ulong)head, first, (ulong)fsts, (ulong)lo, (ulong)hi);
            unit.broken = true;
            return false;
        }
        busy_wait(WAIT_STEP_NS);
    }
    klog(0, "vtd: dmar%lu: an invalidation not done in %lu ms (head %lu, tail %lu)", (ulong)unit.index,
        WAIT_NS / (1000 * 1000), (ulong)(read64(unit, REG_IQH) >> QUEUE_INDEX_SHIFT), unit.queue_tail);
    unit.broken = true;
    return false;
}

bool Vtd::resolve(const Acpi::DmarScope& scope, u8& bus, u8& dev, u8& fn)
{
    /* The bridge a hop names leads to the bus the next hop is on: its
       secondary bus number (PCI-to-PCI Bridge Architecture, 3.2.5.3) */
    static const u16 SECONDARY_BUS = 0x19;
    static const u16 VENDOR_NONE = 0xFFFF;

    auto& pci = Pci::get_instance();
    if (scope.path_length == 0)
        return false;
    bus = scope.start_bus;
    for (size_t h = 0; h < scope.path_length; h++)
    {
        dev = scope.path[h][0];
        fn = scope.path[h][1];
        if (dev > 0x1F || fn > 0x7 || pci.read_word(bus, dev, fn, 0) == VENDOR_NONE)
            return false;
        if (h + 1 < scope.path_length)
            bus = pci.read_byte(bus, dev, fn, SECONDARY_BUS);
    }
    return true;
}

bool Vtd::names_display_alone(const Acpi::DmarUnit& dmar)
{
    static const u16 CLASS_CODE = 0x0B;

    if (dmar.include_all || dmar.scope_count == 0)
        return false;
    auto& pci = Pci::get_instance();
    for (size_t i = 0; i < dmar.scope_count; i++)
    {
        const Acpi::DmarScope& scope = dmar.scope[i];
        u8 bus = 0, dev = 0, fn = 0;
        if (scope.type != Acpi::DMAR_SCOPE_ENDPOINT || !resolve(scope, bus, dev, fn) ||
            pci.read_byte(bus, dev, fn, CLASS_CODE) != Pci::CLS_DISPLAY_CONTROLLER)
            return false;
    }
    return true;
}

u8 Vtd::last_bus()
{
    static const u16 SUBORDINATE_BUS = 0x1A;

    auto& pci = Pci::get_instance();
    u8 last = 0;
    for (ulong i = 0; i < pci.get_device_count(); i++)
    {
        const Pci::DeviceInfo* d = pci.get_device(i);
        if (d == nullptr || !d->valid)
            continue;
        last = stdlib::max<u8>(last, static_cast<u8>(d->bus));
        if (d->base_class == Pci::CLS_BRIDGE_DEVICE && d->sub_class == Pci::SUB_CLS_PCI_BRIDGE)
            last = stdlib::max<u8>(last, pci.read_byte(d->bus, d->slot, d->func, SUBORDINATE_BUS));
    }
    return last;
}

bool Vtd::probe(Unit& unit)
{
    const Acpi::DmarUnit& dmar = *unit.dmar;
    unit.phys = dmar.base;
    unit.span = consts::PAGE_SIZE << stdlib::min<u8>(dmar.size, MAX_REG_SIZE_LOG);
    if (dmar.segment != 0 || (unit.phys & (consts::PAGE_SIZE - 1)) != 0 || unit.phys == 0 ||
        unit.phys > mm::MemoryMap::MAX_PHYS_ADDR - unit.span)
    {
        klog(0, "vtd: dmar%lu at 0x%lX, segment %lu: not a unit to drive", (ulong)unit.index, unit.phys,
            (ulong)dmar.segment);
        return false;
    }

    const ulong va = mm::PageTable::get_instance().map_mmio_region(unit.phys, unit.span);
    if (va == 0)
    {
        klog(0, "vtd: dmar%lu at 0x%lX: its registers could not be mapped", (ulong)unit.index, unit.phys);
        return false;
    }
    unit.regs = reinterpret_cast<volatile u8*>(va);
    unit.version = read32(unit, REG_VER);
    unit.cap = read64(unit, REG_CAP);
    unit.ecap = read64(unit, REG_ECAP);
    unit.graphics = names_display_alone(dmar);

    const u64 sagaw = cap_sagaw(unit.cap);
    unit.width_code = (sagaw & SAGAW_57) ? AW_57 : (sagaw & SAGAW_48) ? AW_48 : (sagaw & SAGAW_39) ? AW_39 : 0;
    unit.leaf_bits = ecap_snoop_control(unit.ecap) ? SL_SNOOP : 0;

    klog(0, "vtd: dmar%lu at 0x%lX, VT-d %lu.%lu, CAP 0x%lX ECAP 0x%lX: %lu domains, %lu-bit DMA%s%s%s%s%s%s",
        (ulong)unit.index, unit.phys, (ulong)((unit.version >> 4) & 0xF), (ulong)(unit.version & 0xF),
        (ulong)unit.cap, (ulong)unit.ecap, (ulong)cap_domains(unit.cap), (ulong)cap_mgaw(unit.cap),
        dmar.include_all ? ", every device" : "", unit.graphics ? ", display devices alone" : "",
        ecap_coherent(unit.ecap) ? ", coherent" : ", not coherent", ecap_intr_remap(unit.ecap) ? ", IR" : "",
        cap_caching_mode(unit.cap) ? ", caching mode" : "", (cap_superpages(unit.cap) & 1) ? ", 2 MiB pages" : "");

    const ulong faults_end = cap_fault_offset(unit.cap) + cap_fault_count(unit.cap) * 2 * sizeof(u64);
    const ulong iotlb_end = ecap_iotlb_offset(unit.ecap) + 2 * sizeof(u64);
    if (faults_end > unit.span || iotlb_end > unit.span || REGS_FIXED_END > unit.span)
    {
        klog(0, "vtd: dmar%lu: registers to 0x%lX, past the 0x%lX its DRHD maps", (ulong)unit.index,
            stdlib::max(faults_end, iotlb_end), unit.span);
        unit.regs = nullptr;
        return false;
    }
    return true;
}

void Vtd::put_right(Unit& unit)
{
    const u32 gsts = read32(unit, REG_GSTS);
    if (gsts & GS_IRE)
    {
        klog(0, "vtd: dmar%lu: interrupt remapping left on by the firmware, turned off", (ulong)unit.index);
        (void)command(unit, GS_IRE, false);
    }
    if (gsts & GS_TE)
    {
        klog(0, "vtd: dmar%lu: translation left on by the firmware, turned off", (ulong)unit.index);
        (void)command(unit, GS_TE, false);
    }
    if (gsts & GS_QIE)
    {
        /* The queue let drain first: a unit turns it off once its head is
           at its tail (6.5.2.5) */
        if (!wait_register(unit, REG_IQH, ~0u, read32(unit, REG_IQT)))
            klog(0, "vtd: dmar%lu: the firmware's invalidation queue did not drain", (ulong)unit.index);
        (void)command(unit, GS_QIE, false);
    }
    const u32 pmen = read32(unit, REG_PMEN);
    if (pmen & PMEN_EPM)
    {
        klog(0, "vtd: dmar%lu: protected memory regions left on by the firmware, turned off", (ulong)unit.index);
        write32(unit, REG_PMEN, pmen & ~PMEN_EPM);
        if (!wait_register(unit, REG_PMEN, PMEN_PRS, 0))
            klog(0, "vtd: dmar%lu: the protected memory regions are still on", (ulong)unit.index);
    }
}

void Vtd::free_tables(Unit& unit)
{
    for (size_t bus = 0; bus < BUSES; bus++)
    {
        if (unit.context[bus] != nullptr)
            mm::unmap_free_pages(unit.context[bus]);
        unit.context[bus] = nullptr;
        unit.context_phys[bus] = 0;
    }
    if (unit.root != nullptr)
        mm::unmap_free_pages(unit.root);
    if (unit.queue != nullptr)
        mm::unmap_free_pages(unit.queue);
    if (unit.status != nullptr)
        mm::unmap_free_pages(const_cast<u32*>(unit.status));
    unit.root = nullptr;
    unit.queue = nullptr;
    unit.status = nullptr;
}

bool Vtd::take_over(Unit& unit)
{
    if (!ecap_queued(unit.ecap) || !ecap_pass_through(unit.ecap) || unit.width_code == 0)
    {
        klog(0, "vtd: dmar%lu cannot be driven: %s", (ulong)unit.index, !ecap_queued(unit.ecap) ?
            "no queued invalidation" : !ecap_pass_through(unit.ecap) ? "no pass-through" :
            "no table width this builds");
        return false;
    }

    /* The tables, each a page the unit reads, zeroed: the root, a context
       table a bus with every device and function passed through, the
       queue and the page its waits write their status into */
    unit.root = static_cast<u64*>(mm::alloc_map_pages(1, &unit.root_phys));
    unit.queue = static_cast<u64*>(mm::alloc_map_pages(1, &unit.queue_phys));
    unit.status = static_cast<volatile u32*>(mm::alloc_map_pages(1, &unit.status_phys));
    if (unit.root == nullptr || unit.queue == nullptr || unit.status == nullptr)
    {
        klog(0, "vtd: dmar%lu: no memory for its tables", (ulong)unit.index);
        free_tables(unit);
        return false;
    }
    stdlib::mem_set(unit.root, 0, consts::PAGE_SIZE);
    stdlib::mem_set(unit.queue, 0, consts::PAGE_SIZE);
    stdlib::mem_set(const_cast<u32*>(unit.status), 0, consts::PAGE_SIZE);

    const u64 context_hi = static_cast<u64>(unit.width_code) | (DID_PASS_THROUGH << CONTEXT_DID_SHIFT);
    for (size_t bus = 0; bus <= last_bus_; bus++)
    {
        u64* table = static_cast<u64*>(mm::alloc_map_pages(1, &unit.context_phys[bus]));
        if (table == nullptr)
        {
            klog(0, "vtd: dmar%lu: no memory for bus %lu's context table", (ulong)unit.index, (ulong)bus);
            free_tables(unit);
            return false;
        }
        unit.context[bus] = table;
        for (size_t devfn = 0; devfn < DEVFNS; devfn++)
        {
            table[devfn * ENTRY_WORDS] = ENTRY_PRESENT | CONTEXT_PASS_THROUGH;
            table[devfn * ENTRY_WORDS + 1] = context_hi;
        }
        flush(unit, table, consts::PAGE_SIZE);
        unit.root[bus * ENTRY_WORDS] = unit.context_phys[bus] | ENTRY_PRESENT;
    }
    flush(unit, unit.root, consts::PAGE_SIZE);

    /* The root table's pointer set -- legacy mode, TTM 00 -- then the queue
       up, then everything any cache of the unit's may hold of the
       firmware's tables dropped (6.5.2): only then is translation turned
       on, so that no request is translated through what nos did not make */
    write64(unit, REG_RTADDR, unit.root_phys);
    if (!command(unit, GS_SRTP, true))
    {
        free_tables(unit);
        return false;
    }

    unit.queue_tail = 0;
    write64(unit, REG_IQT, 0);
    write64(unit, REG_IQA, unit.queue_phys);
    if (!command(unit, GS_QIE, true))
    {
        free_tables(unit);
        return false;
    }

    if (!invalidate(unit, INV_CONTEXT_GLOBAL, 0) || !invalidate(unit, INV_IOTLB_GLOBAL, 0))
    {
        (void)command(unit, GS_QIE, false);
        free_tables(unit);
        return false;
    }

    /* The fault event: this CPU's, at FAULT_VECTOR -- left masked where its
       APIC id does not fit the message, its faults then found by `iommu` */
    unit.fault_irq = fault_apic_id_ <= MSI_DEST_LIMIT;
    write32(unit, REG_FEDATA, FAULT_VECTOR);
    write32(unit, REG_FEADDR, MSI_ADDRESS_BASE | (fault_apic_id_ << MSI_DEST_SHIFT));
    write32(unit, REG_FEUADDR, 0);
    write32(unit, REG_FECTL, unit.fault_irq ? 0 : FECTL_IM);

    if (cap_rwbf(unit.cap) && !flush_write_buffer(unit))
    {
        write32(unit, REG_FECTL, FECTL_IM);
        (void)command(unit, GS_QIE, false);
        free_tables(unit);
        return false;
    }

    if (!command(unit, GS_TE, true))
    {
        write32(unit, REG_FECTL, FECTL_IM);
        (void)command(unit, GS_QIE, false);
        free_tables(unit);
        return false;
    }
    unit.taken = true;
    unit.state = "translating, every device passed through";

    klog(0, "vtd: dmar%lu: translating, every device of buses 0-%lu passed through, queued invalidation%s",
        (ulong)unit.index, (ulong)last_bus_, unit.fault_irq ? ", faults at the boot CPU" : ", faults not interrupted");
    return true;
}

void Vtd::release(Unit& unit)
{
    if (!unit.taken)
        return;
    write32(unit, REG_FECTL, FECTL_IM);
    (void)command(unit, GS_TE, false);
    (void)command(unit, GS_QIE, false);
    free_tables(unit);
    unit.taken = false;
    unit.state = "given back";
}

void Vtd::setup()
{
    auto& acpi = Acpi::get_instance();
    if (!acpi.has_dmar())
    {
        klog(ACPI_LL, "vtd: no DMAR: no remapping units");
        return;
    }

    const bool wanted = Parameters::get_instance().is_iommu_on();
    last_bus_ = last_bus();
    fault_apic_id_ = static_cast<u32>(hal::get_current_cpu_hw_id());

    for (size_t i = 0; i < acpi.get_dmar_unit_count() && unit_count_ < stdlib::array_size(units_); i++)
    {
        Unit* unit = mm::talloc<Unit, TAG>();
        if (unit == nullptr)
        {
            klog(0, "vtd: no memory for unit %lu", (ulong)i);
            break;
        }
        unit->dmar = acpi.get_dmar_unit(i);
        unit->index = static_cast<u8>(i);
        unit->phys = 0;
        unit->span = 0;
        unit->regs = nullptr;
        unit->version = 0;
        unit->cap = 0;
        unit->ecap = 0;
        unit->taken = false;
        unit->graphics = false;
        unit->width_code = 0;
        unit->root = nullptr;
        unit->root_phys = 0;
        unit->queue = nullptr;
        unit->queue_phys = 0;
        unit->status = nullptr;
        unit->status_phys = 0;
        unit->status_seq = 0;
        unit->broken = false;
        unit->queue_tail = 0;
        unit->faults = 0;
        unit->faults_unsaid = 0;
        unit->rate_second = 0;
        unit->rate_count = 0;
        unit->storm = false;
        unit->fault_irq = false;
        unit->state = "as the firmware left it";
        for (size_t w = 0; w < MAX_DIDS_WORDS; w++)
            unit->dids[w] = 0;
        unit->leaf_bits = 0;
        for (size_t bus = 0; bus < BUSES; bus++)
        {
            unit->context[bus] = nullptr;
            unit->context_phys[bus] = 0;
        }
        units_[unit_count_++] = unit;
        if (!probe(*unit))
        {
            unit->state = "not driven: its registers are not to be read";
            continue;
        }
        put_right(*unit);
    }

    if (!wanted)
    {
        klog(0, "vtd: %lu units, none translating (iommu=on turns them on)", (ulong)unit_count_);
        return;
    }
    if (!acpi.dmar_complete() || unit_count_ != acpi.get_dmar_unit_count())
    {
        klog(0, "vtd: iommu=on, but the DMAR is not all kept: no unit turned on");
        return;
    }

    for (size_t i = 0; i < unit_count_; i++)
    {
        if (units_[i]->regs == nullptr)
        {
            klog(0, "vtd: iommu=on, but dmar%lu cannot be driven: no unit turned on", (ulong)i);
            return;
        }
    }

    /* Before the first unit can send one */
    Idt::get_instance().set_descriptor(FAULT_VECTOR, IdtDescriptor::encode(vtd_fault_interrupt_stub));

    for (size_t i = 0; i < unit_count_; i++)
    {
        Unit& unit = *units_[i];
        if (unit.graphics)
        {
            unit.state = "left be: display devices alone";
            continue;
        }
        if (!take_over(unit))
        {
            klog(0, "vtd: dmar%lu did not come up: every unit given back", (ulong)i);
            for (size_t j = 0; j < i; j++)
                release(*units_[j]);
            unit.state = "did not come up";
            return;
        }
    }
    on_ = true;
}

bool Vtd::is_on() const
{
    return on_;
}

const char* Vtd::fault_reason(u8 reason)
{
    switch (reason)
    {
    case 0x1: return "no root entry";
    case 0x2: return "no context entry";
    case 0x3: return "context entry invalid";
    case 0x4: return "address past the domain's width";
    case 0x5: return "write to a page not mapped writable";
    case 0x6: return "read of a page not mapped readable";
    case 0x7: return "a paging entry could not be read";
    case 0x8: return "the root table could not be read";
    case 0x9: return "a context table could not be read";
    case 0xA: return "reserved bit in a root entry";
    case 0xB: return "reserved bit in a context entry";
    case 0xC: return "reserved bit in a page table entry";
    case 0xD: return "translation type blocked";
    case 0x20: return "reserved bit in an interrupt request";
    case 0x21: return "interrupt index past the table";
    case 0x22: return "interrupt entry not present";
    case 0x23: return "interrupt table not readable";
    case 0x24: return "reserved bit in an interrupt entry";
    case 0x25: return "compatibility-format interrupt blocked";
    case 0x26: return "interrupt from a requester not its entry's";
    default: return "reason not named here";
    }
}

void Vtd::report_faults(Unit& unit)
{
    const ulong offset = cap_fault_offset(unit.cap);
    const ulong count = cap_fault_count(unit.cap);
    const ulong second = get_boot_time().get_secs();
    if (second != unit.rate_second)
    {
        if (unit.faults_unsaid != 0)
            klog(0, "vtd: dmar%lu: %lu more faults, not said", (ulong)unit.index, unit.faults_unsaid);
        unit.rate_second = second;
        unit.rate_count = 0;
        unit.faults_unsaid = 0;
    }

    const u32 fsts = read32(unit, REG_FSTS);
    if (fsts & FSTS_PPF)
    {
        ulong index = (fsts >> FSTS_FRI_SHIFT) & FSTS_FRI_MASK;
        for (ulong n = 0; n < count; n++, index = (index + 1) % count)
        {
            const ulong reg = offset + (index % count) * 2 * sizeof(u64);
            const u64 high = read64(unit, reg + FRCD_HIGH);
            if (!(high & FRCD_F))
                break;
            const u64 low = read64(unit, reg);
            write32(unit, reg + FRCD_HIGH_UPPER, FRCD_F_UPPER);

            unit.faults++;
            unit.rate_count++;
            if (unit.rate_count > FAULTS_SAID_A_SECOND)
            {
                unit.faults_unsaid++;
                continue;
            }
            const u16 sid = static_cast<u16>(high & FRCD_SID_MASK);
            const u8 reason = static_cast<u8>(high >> FRCD_REASON_SHIFT);
            klog(0, "vtd: dmar%lu: %s of 0x%lX by %02lx:%02lx.%lu blocked: 0x%02lx, %s", (ulong)unit.index,
                (high & FRCD_READ) ? "read" : "write", (ulong)(low & FRCD_ADDRESS_MASK), (ulong)sid_bus(sid),
                (ulong)sid_dev(sid), (ulong)sid_fn(sid), (ulong)reason, fault_reason(reason));
        }
    }

    const u32 cleared = fsts & FSTS_CLEARED;
    if (cleared != 0)
    {
        if (cleared & FSTS_PFO)
            klog(0, "vtd: dmar%lu: faults past what its registers record were lost", (ulong)unit.index);
        if (cleared & (FSTS_IQE | FSTS_ICE | FSTS_ITE))
            klog(0, "vtd: dmar%lu: an invalidation error, FSTS 0x%lX", (ulong)unit.index, (ulong)fsts);
        write32(unit, REG_FSTS, cleared);
    }

    if (unit.rate_count > FAULT_STORM && !unit.storm && unit.fault_irq)
    {
        unit.storm = true;
        write32(unit, REG_FECTL, FECTL_IM);
        klog(0, "vtd: dmar%lu: %lu faults this second: its fault event masked until `iommu` looks again",
            (ulong)unit.index, unit.rate_count);
    }
}

void Vtd::on_fault_interrupt()
{
    stdlib::AutoLock lock(fault_lock_);
    for (size_t i = 0; i < unit_count_; i++)
    {
        if (units_[i]->fault_irq)
            report_faults(*units_[i]);
    }
}

void Vtd::print(stdlib::Printer& printer)
{
    if (unit_count_ == 0)
    {
        printer.printf("no DMA remapping units\n");
        return;
    }
    printer.printf("iommu: %s\n", on_ ? "on" : "off");
    for (size_t i = 0; i < unit_count_; i++)
    {
        Unit& unit = *units_[i];
        if (unit.taken)
        {
            stdlib::AutoLock lock(fault_lock_);
            report_faults(unit);
            if (unit.storm)
            {
                unit.storm = false;
                write32(unit, REG_FECTL, 0);
            }
        }
        printer.printf("dmar%lu 0x%lX: %s\n", (ulong)unit.index, unit.phys, unit.state);
        if (unit.regs == nullptr)
            continue;
        printer.printf("  VT-d %lu.%lu  CAP 0x%lX  ECAP 0x%lX  GSTS 0x%lX\n", (ulong)((unit.version >> 4) & 0xF),
            (ulong)(unit.version & 0xF), (ulong)unit.cap, (ulong)unit.ecap, (ulong)read32(unit, REG_GSTS));
        printer.printf("  %lu domains, %lu-bit, %s%s%s%s%s\n", (ulong)cap_domains(unit.cap),
            (ulong)cap_mgaw(unit.cap), ecap_coherent(unit.ecap) ? "coherent" : "not coherent",
            ecap_queued(unit.ecap) ? ", QI" : "", ecap_intr_remap(unit.ecap) ? ", IR" : "",
            ecap_x2apic(unit.ecap) ? ", x2APIC" : "", ecap_snoop_control(unit.ecap) ? ", snoop control" : "");
        printer.printf("  faults %lu%s\n", unit.faults, unit.broken ? "; its invalidation queue stopped" : "");
    }
}

bool Vtd::parse_bdf(const char* s, u8& bus, u8& dev, u8& fn)
{
    ulong fields[3] = {0, 0, 0};
    const char separators[] = {':', '.', '\0'};
    for (size_t f = 0; f < stdlib::array_size(fields); f++)
    {
        size_t digits = 0;
        for (; *s != '\0' && *s != separators[f] && *s != ' '; s++, digits++)
        {
            const char c = *s;
            const ulong d = (c >= '0' && c <= '9') ? (ulong)(c - '0') : (c >= 'a' && c <= 'f') ?
                (ulong)(c - 'a' + 10) : (c >= 'A' && c <= 'F') ? (ulong)(c - 'A' + 10) : 16;
            if (d > 15 || digits >= 2)
                return false;
            fields[f] = fields[f] * 16 + d;
        }
        if (digits == 0 || (f + 1 < stdlib::array_size(fields) && *s++ != separators[f]))
            return false;
    }
    if (fields[1] > 0x1F || fields[2] > 0x7)
        return false;
    bus = static_cast<u8>(fields[0]);
    dev = static_cast<u8>(fields[1]);
    fn = static_cast<u8>(fields[2]);
    return true;
}

bool Vtd::set_blocked(u8 bus, u8 dev, u8 fn, bool blocked, stdlib::Printer& printer)
{
    if (!on_)
    {
        printer.printf("iommu: DMA is not translated (iommu=on turns it on)\n");
        return false;
    }
    if (bus > last_bus_)
    {
        printer.printf("iommu: bus %02lx has no context table: its devices' DMA is blocked already\n", (ulong)bus);
        return false;
    }

    const u16 sid = static_cast<u16>((static_cast<u16>(bus) << SID_BUS_SHIFT) | (dev << 3) | fn);
    stdlib::AutoLock lock(domains_lock_);
    Unit* unit = unit_for(sid);
    if (unit == nullptr || domain_holding(sid) != nullptr)
    {
        printer.printf("iommu: %02lx:%02lx.%lx is %s\n", (ulong)bus, (ulong)dev, (ulong)fn,
            unit == nullptr ? "behind no unit taken over" : "a domain's: its owner blocks it");
        return false;
    }
    const bool ok = blocked ? write_context(*unit, sid, 0, 0, DID_PASS_THROUGH) :
        write_context(*unit, sid, ENTRY_PRESENT | CONTEXT_PASS_THROUGH,
            static_cast<u64>(unit->width_code) | (DID_PASS_THROUGH << CONTEXT_DID_SHIFT), DID_PASS_THROUGH);
    printer.printf("iommu: %02lx:%02lx.%lx %s%s\n", (ulong)bus, (ulong)dev, (ulong)fn,
        blocked ? "blocked" : "passed through", ok ? "" : ", but the unit did not take it: see the log");
    return ok;
}

Vtd::Unit* Vtd::unit_for(u16 sid)
{
    Unit* every = nullptr;
    for (size_t i = 0; i < unit_count_; i++)
    {
        Unit& unit = *units_[i];
        const Acpi::DmarUnit& dmar = *unit.dmar;
        if (dmar.include_all)
        {
            every = &unit;
            continue;
        }
        for (size_t s = 0; s < dmar.scope_count; s++)
        {
            if (scope_covers(dmar.scope[s], sid))
                return unit.taken ? &unit : nullptr;
        }
    }
    return (every != nullptr && every->taken) ? every : nullptr;
}

bool Vtd::scope_covers(const Acpi::DmarScope& scope, u16 sid)
{
    static const u16 SECONDARY_BUS = 0x19;
    static const u16 SUBORDINATE_BUS = 0x1A;

    u8 bus = 0, dev = 0, fn = 0;
    if ((scope.type != Acpi::DMAR_SCOPE_ENDPOINT && scope.type != Acpi::DMAR_SCOPE_BRIDGE) ||
        !resolve(scope, bus, dev, fn))
        return false;
    const u16 at = static_cast<u16>((static_cast<u16>(bus) << SID_BUS_SHIFT) | (dev << 3) | fn);
    if (at == sid)
        return true;
    if (scope.type != Acpi::DMAR_SCOPE_BRIDGE)
        return false;
    auto& pci = Pci::get_instance();
    const u8 secondary = pci.read_byte(bus, dev, fn, SECONDARY_BUS);
    const u8 subordinate = pci.read_byte(bus, dev, fn, SUBORDINATE_BUS);
    return sid_bus(sid) >= secondary && sid_bus(sid) <= subordinate;
}

bool Vtd::write_context(Unit& unit, u16 sid, u64 lo, u64 hi, u16 old_did)
{
    const u8 bus = sid_bus(sid);
    if (bus > last_bus_ || unit.context[bus] == nullptr)
        return false;
    u64* entry = unit.context[bus] + static_cast<size_t>(sid & SID_DEVFN_MASK) * ENTRY_WORDS;

    /* Not present first, and the unit told, so that it never walks an entry
       half one thing and half another: a context entry is two words */
    entry[0] = 0;
    entry[1] = 0;
    flush(unit, entry, ENTRY_WORDS * sizeof(u64));
    if (!invalidate(unit, inv_context_device(old_did, sid), 0) || !invalidate(unit, inv_iotlb_domain(old_did), 0))
        return false;
    if (!(lo & ENTRY_PRESENT))
        return true;

    /* Then the new one, its present bit -- in the low word -- last; and the
       unit told again, which a unit in caching mode needs for a not-present
       entry made present, and QEMU's learns the domain's pages by */
    entry[1] = hi;
    hal::compiler_barrier();
    entry[0] = lo;
    flush(unit, entry, ENTRY_WORDS * sizeof(u64));
    const u16 did = static_cast<u16>((hi >> CONTEXT_DID_SHIFT) & CONTEXT_DID_MASK);
    return invalidate(unit, inv_context_device(did, sid), 0) && invalidate(unit, inv_iotlb_domain(did), 0);
}

Vtd::Domain* Vtd::domain_of(ulong handle)
{
    return (handle >= 1 && handle <= MAX_DOMAINS) ? domains_[handle - 1] : nullptr;
}

Vtd::Domain* Vtd::domain_holding(u16 sid)
{
    for (size_t i = 0; i < MAX_DOMAINS; i++)
    {
        Domain* d = domains_[i];
        if (d == nullptr)
            continue;
        for (size_t k = 0; k < d->device_count; k++)
        {
            if (d->devices[k] == sid)
                return d;
        }
    }
    return nullptr;
}

Vtd::Table* Vtd::make_table(bool last)
{
    Table* table = mm::talloc<Table, TAG>();
    if (table == nullptr)
        return nullptr;
    table->child = nullptr;
    table->entries = static_cast<u64*>(mm::alloc_map_pages(1, &table->phys));
    if (table->entries == nullptr)
    {
        mm::free(table);
        return nullptr;
    }
    stdlib::mem_set(table->entries, 0, consts::PAGE_SIZE);
    if (!last)
    {
        table->child = static_cast<Table**>(mm::alloc(TABLE_ENTRIES * sizeof(Table*), TAG));
        if (table->child == nullptr)
        {
            mm::unmap_free_pages(table->entries);
            mm::free(table);
            return nullptr;
        }
        stdlib::mem_set(table->child, 0, TABLE_ENTRIES * sizeof(Table*));
    }
    return table;
}

void Vtd::free_table(Table* table, size_t level)
{
    if (table == nullptr)
        return;
    if (table->child != nullptr)
    {
        for (size_t i = 0; i < TABLE_ENTRIES; i++)
            free_table(table->child[i], level - 1);
        mm::free(table->child);
    }
    mm::unmap_free_pages(table->entries);
    mm::free(table);
}

bool Vtd::map_leaf(Domain& domain, u64 iova, u64 phys, ulong shift)
{
    const size_t leaf_level = (shift - SHIFT_4K) / LEVEL_BITS;
    Table* table = domain.root;
    for (size_t level = domain.levels - 1u; ; level--)
    {
        const size_t index = (iova >> (SHIFT_4K + LEVEL_BITS * level)) & (TABLE_ENTRIES - 1);
        u64& entry = table->entries[index];
        if (level == leaf_level)
        {
            /* A page mapped is never mapped over: what a device may reach is
               only ever what was asked for once */
            if (entry & (SL_READ | SL_WRITE))
                return false;
            entry = (phys & SL_ADDRESS) | SL_READ | SL_WRITE | domain.unit->leaf_bits |
                (level > 0 ? SL_PAGE_SIZE : 0);
            flush(*domain.unit, &entry, sizeof(u64));
            return true;
        }
        Table* next = table->child[index];
        if (next == nullptr)
        {
            /* A leaf where a table is wanted: mapped already */
            if (entry & (SL_READ | SL_WRITE))
                return false;
            next = make_table(level - 1 == 0);
            if (next == nullptr)
                return false;
            /* Zeroed and in memory before the entry makes it reachable */
            flush(*domain.unit, next->entries, consts::PAGE_SIZE);
            table->child[index] = next;
            entry = next->phys | SL_READ | SL_WRITE;
            flush(*domain.unit, &entry, sizeof(u64));
        }
        table = next;
    }
}

ulong Vtd::domain_create(u16 sid)
{
    stdlib::AutoLock lock(domains_lock_);
    if (!on_)
        return 0;
    Unit* unit = unit_for(sid);
    if (unit == nullptr)
    {
        klog(0, "vtd: no domain for %02lx:%02lx.%lu: behind no unit taken over", (ulong)sid_bus(sid),
            (ulong)sid_dev(sid), (ulong)sid_fn(sid));
        return 0;
    }

    size_t slot = MAX_DOMAINS;
    for (size_t i = 0; i < MAX_DOMAINS && slot == MAX_DOMAINS; i++)
    {
        if (domains_[i] == nullptr)
            slot = i;
    }
    const u64 sagaw = cap_sagaw(unit->cap);
    const u8 levels = (sagaw & SAGAW_48) ? 4 : (sagaw & SAGAW_39) ? 3 : 0;
    const size_t dids = stdlib::min<size_t>(static_cast<size_t>(cap_domains(unit->cap)), MAX_DIDS);
    size_t did = dids;
    for (size_t d = FIRST_DID; d < dids && did == dids; d++)
    {
        if (!(unit->dids[d / 64] & (1ULL << (d % 64))))
            did = d;
    }
    if (slot == MAX_DOMAINS || levels == 0 || did == dids)
    {
        klog(0, "vtd: dmar%lu: no domain: %s", (ulong)unit->index, slot == MAX_DOMAINS ? "every slot taken" :
            levels == 0 ? "no table width this builds" : "every domain id taken");
        return 0;
    }

    Domain* domain = mm::talloc<Domain, TAG>();
    Table* root = (domain != nullptr) ? make_table(false) : nullptr;
    if (root == nullptr)
    {
        if (domain != nullptr)
            mm::free(domain);
        return 0;
    }
    flush(*unit, root->entries, consts::PAGE_SIZE);
    domain->unit = unit;
    domain->did = static_cast<u16>(did);
    domain->levels = levels;
    domain->root = root;
    domain->device_count = 0;
    for (size_t k = 0; k < MAX_DOMAIN_DEVICES; k++)
        domain->devices[k] = 0;
    unit->dids[did / 64] |= 1ULL << (did % 64);
    domains_[slot] = domain;
    klog(0, "vtd: dmar%lu: domain %lu, a %lu-level table", (ulong)unit->index, (ulong)did, (ulong)levels);
    return slot + 1;
}

bool Vtd::domain_map(ulong handle, u64 iova, u64 phys, ulong pages)
{
    stdlib::AutoLock lock(domains_lock_);
    Domain* domain = domain_of(handle);
    if (domain == nullptr || pages == 0 || ((iova | phys) & (consts::PAGE_SIZE - 1)) != 0 ||
        (phys & ~SL_ADDRESS) != 0)
        return false;
    const ulong width_pages = 1UL << (LEVEL_BITS * domain->levels);
    if (pages > width_pages || (iova >> SHIFT_4K) > width_pages - pages ||
        (phys >> SHIFT_4K) > (SL_ADDRESS >> SHIFT_4K) - pages + 1)
        return false;

    const u64 bytes = static_cast<u64>(pages) << SHIFT_4K;
    const u64 huge = 1ULL << SHIFT_2M;
    const bool big = (cap_superpages(domain->unit->cap) & SLLPS_2M) != 0;
    for (u64 done = 0; done < bytes; )
    {
        const u64 at = iova + done;
        const u64 to = phys + done;
        const ulong shift = (big && ((at | to) & (huge - 1)) == 0 && bytes - done >= huge) ? SHIFT_2M : SHIFT_4K;
        if (!map_leaf(*domain, at, to, shift))
        {
            klog(0, "vtd: domain %lu: 0x%lX not mapped to 0x%lX: mapped already, or no memory for a table",
                (ulong)domain->did, (ulong)at, (ulong)to);
            return false;
        }
        done += 1ULL << shift;
    }
    /* A unit in caching mode caches what is not present too; one with a
       device in the domain may have walked these addresses before */
    if (domain->device_count > 0 || cap_caching_mode(domain->unit->cap))
        return invalidate(*domain->unit, inv_iotlb_domain(domain->did), 0);
    return true;
}

bool Vtd::domain_attach(ulong handle, u16 sid)
{
    stdlib::AutoLock lock(domains_lock_);
    Domain* domain = domain_of(handle);
    if (domain == nullptr || unit_for(sid) != domain->unit || domain_holding(sid) != nullptr ||
        domain->device_count >= MAX_DOMAIN_DEVICES)
        return false;
    const u64 lo = domain->root->phys | ENTRY_PRESENT | CONTEXT_UNTRANSLATED;
    const u64 hi = static_cast<u64>(domain->levels == 4 ? AW_48 : AW_39) |
        (static_cast<u64>(domain->did) << CONTEXT_DID_SHIFT);
    if (!write_context(*domain->unit, sid, lo, hi, DID_PASS_THROUGH))
        return false;
    domain->devices[domain->device_count++] = sid;
    klog(0, "vtd: %02lx:%02lx.%lu in domain %lu", (ulong)sid_bus(sid), (ulong)sid_dev(sid), (ulong)sid_fn(sid),
        (ulong)domain->did);
    return true;
}

bool Vtd::domain_detach(ulong handle, u16 sid)
{
    stdlib::AutoLock lock(domains_lock_);
    Domain* domain = domain_of(handle);
    if (domain == nullptr)
        return false;
    for (size_t k = 0; k < domain->device_count; k++)
    {
        if (domain->devices[k] != sid)
            continue;
        /* Out of the domain whatever the unit says: the entry is not present
           either way, and a unit that did not take the change has said so */
        const bool ok = write_context(*domain->unit, sid, 0, 0, domain->did);
        domain->devices[k] = domain->devices[--domain->device_count];
        klog(0, "vtd: %02lx:%02lx.%lu out of domain %lu, blocked", (ulong)sid_bus(sid), (ulong)sid_dev(sid),
            (ulong)sid_fn(sid), (ulong)domain->did);
        return ok;
    }
    return false;
}

void Vtd::domain_destroy(ulong handle)
{
    stdlib::AutoLock lock(domains_lock_);
    Domain* domain = domain_of(handle);
    if (domain == nullptr)
        return;
    while (domain->device_count > 0)
        (void)write_context(*domain->unit, domain->devices[--domain->device_count], 0, 0, domain->did);
    /* Nothing names the table now, and no unit holds anything of it: the
       IOTLB of the domain dropped once more, then the pages let go of */
    (void)invalidate(*domain->unit, inv_iotlb_domain(domain->did), 0);
    free_table(domain->root, domain->levels - 1u);
    domain->unit->dids[domain->did / 64] &= ~(1ULL << (domain->did % 64));
    klog(0, "vtd: dmar%lu: domain %lu gone", (ulong)domain->unit->index, (ulong)domain->did);
    domains_[handle - 1] = nullptr;
    mm::free(domain);
}

bool Vtd::release(u16 sid)
{
    stdlib::AutoLock lock(domains_lock_);
    Unit* unit = unit_for(sid);
    if (unit == nullptr || domain_holding(sid) != nullptr)
        return false;
    return write_context(*unit, sid, ENTRY_PRESENT | CONTEXT_PASS_THROUGH,
        static_cast<u64>(unit->width_code) | (DID_PASS_THROUGH << CONTEXT_DID_SHIFT), DID_PASS_THROUGH);
}

void Vtd::shell(const char* args, stdlib::Printer& printer)
{
    const char* end = nullptr;
    const char* word = stdlib::next_token(args, end);
    if (word == nullptr)
    {
        print(printer);
        return;
    }

    const size_t length = static_cast<size_t>(end - word);
    const bool block = length == 5 && stdlib::strn_cmp(word, "block", 5) == 0;
    const bool unblock = length == 7 && stdlib::strn_cmp(word, "unblock", 7) == 0;
    const bool identity = length == 8 && stdlib::strn_cmp(word, "identity", 8) == 0;
    const bool give_back = length == 7 && stdlib::strn_cmp(word, "release", 7) == 0;
    u8 bus = 0, dev = 0, fn = 0;
    const char* bdf_end = nullptr;
    const char* bdf = (block || unblock || identity || give_back) ? stdlib::next_token(end, bdf_end) : nullptr;
    if (bdf == nullptr || !parse_bdf(bdf, bus, dev, fn))
    {
        printer.printf("usage: iommu [block|unblock|identity|release <bus>:<dev>.<fn>]\n");
        return;
    }
    if (block || unblock)
    {
        (void)set_blocked(bus, dev, fn, block, printer);
        return;
    }

    const u16 sid = static_cast<u16>((static_cast<u16>(bus) << SID_BUS_SHIFT) | (dev << 3) | fn);
    size_t slot = MAX_SHELL_DOMAINS;
    for (size_t i = 0; i < MAX_SHELL_DOMAINS; i++)
    {
        if (shell_domains_[i].handle != 0 && shell_domains_[i].sid == sid)
            slot = i;
    }
    if (give_back)
    {
        if (slot == MAX_SHELL_DOMAINS)
        {
            printer.printf("iommu: %02lx:%02lx.%lx is in no domain `iommu identity` made\n", (ulong)bus, (ulong)dev,
                (ulong)fn);
            return;
        }
        domain_destroy(shell_domains_[slot].handle);
        shell_domains_[slot].handle = 0;
        const bool ok = release(sid);
        printer.printf("iommu: %02lx:%02lx.%lx %s\n", (ulong)bus, (ulong)dev, (ulong)fn,
            ok ? "out of its domain, passed through" : "out of its domain, but still blocked: see the log");
        return;
    }

    /* identity: every page of usable RAM mapped to itself */
    for (size_t i = 0; i < MAX_SHELL_DOMAINS && slot == MAX_SHELL_DOMAINS; i++)
    {
        if (shell_domains_[i].handle == 0)
            slot = i;
    }
    const ulong handle = (slot == MAX_SHELL_DOMAINS) ? 0 : domain_create(sid);
    if (handle == 0)
    {
        printer.printf("iommu: no domain for %02lx:%02lx.%lx: see the log\n", (ulong)bus, (ulong)dev, (ulong)fn);
        return;
    }
    auto& map = mm::MemoryMap::get_instance();
    ulong mapped = 0;
    bool ok = true;
    for (size_t i = 0; ok && i < map.get_region_count(); i++)
    {
        ulong addr = 0, len = 0, type = 0;
        if (!map.get_region(i, addr, len, type) || type != mm::MemoryMap::USABLE_RAM_TYPE)
            continue;
        const ulong start = stdlib::round_up(addr, consts::PAGE_SIZE);
        const ulong stop = stdlib::round_down(addr + len, consts::PAGE_SIZE);
        if (stop <= start)
            continue;
        ok = domain_map(handle, start, start, (stop - start) / consts::PAGE_SIZE);
        mapped += stop - start;
    }
    if (!ok || !domain_attach(handle, sid))
    {
        domain_destroy(handle);
        printer.printf("iommu: %02lx:%02lx.%lx not moved: its domain did not come up, see the log\n", (ulong)bus,
            (ulong)dev, (ulong)fn);
        (void)release(sid);
        return;
    }
    shell_domains_[slot].sid = sid;
    shell_domains_[slot].handle = handle;
    printer.printf("iommu: %02lx:%02lx.%lx in a domain of its own, %lu MiB of RAM mapped to itself\n", (ulong)bus,
        (ulong)dev, (ulong)fn, mapped >> 20);
}

extern "C" void vtd_fault_interrupt(Context* ctx)
{
    (void)ctx;
    Vtd::get_instance().on_fault_interrupt();
    hal::irq_eoi(Vtd::FAULT_VECTOR);
}

}
