#include "acpi.h"

#include <kernel/klog.h>
#include <kernel/cpu.h>
#include <kernel/numa.h>
#include <mm/memory_map.h>
#include <mm/page_table.h>
#include <arch/x86_64/grub.h>

namespace kernel
{

Acpi::Acpi()
    : root_(nullptr)
    , root_length_(0)
    , root_is_xsdt_(false)
    , lapic_address_(nullptr)
    , io_apic_address_(nullptr)
    , irq_to_gsi_size_(0)
    , io_apic_gsi_base_(~0U)
    , io_apic_id_(0)
    , pm1a_cnt_port_(0)
    , pm1b_cnt_port_(0)
    , dsdt_phys_(0)
    , s5_valid_(false)
    , s5_typ_a_(0)
    , s5_typ_b_(0)
    , reset_reg_valid_(false)
    , reset_reg_port_(0)
    , reset_val_(0)
    , century_register_(0)
    , hpet_base_phys_(0)
    , hpet_min_tick_(0)
    , firmware_watchdog_(false)
    , dmar_present_(false)
    , dmar_complete_(false)
    , dmar_width_(0)
    , dmar_flags_(0)
    , dmar_unit_count_(0)
    , dmar_reserved_count_(0)
    , mcfg_base_(0)
    , mcfg_start_bus_(0)
    , mcfg_end_bus_(0)
{
    oem_id_[0] = '\0';
    for (size_t i = 0; i < stdlib::array_size(table_); i++)
    {
        table_[i] = nullptr;
        table_length_[i] = 0;
    }
    stdlib::mem_set(dmar_unit_, 0, sizeof(dmar_unit_));
    stdlib::mem_set(dmar_reserved_, 0, sizeof(dmar_reserved_));
}

Acpi::~Acpi()
{
}

int Acpi::compute_sum(void* table, size_t len)
{
    u8* p = reinterpret_cast<u8*>(table);

    ulong sum = 0;
    for (size_t i = 0; i < len; i++)
    {
        sum += *p++;
    }

    return sum & 0xFF;
}

bool Acpi::parse_rsdp(RSDPDescriptor20 *rsdp, ulong& root_phys_addr, bool& is_xsdt)
{
    if (compute_sum(rsdp, sizeof(rsdp->first_part)) != 0)
    {
        klog(0, "Rsdp 0x%p checksum failed (sum 0x%lX)",
            rsdp, (ulong)compute_sum(rsdp, sizeof(rsdp->first_part)));
        return false;
    }

    /* ACPI 2.0+: the extended checksum covers the whole descriptor */
    if (rsdp->first_part.revision >= 2 &&
        compute_sum(rsdp, sizeof(*rsdp)) != 0)
    {
        klog(0, "Rsdp 0x%p extended checksum failed (sum 0x%lX)",
            rsdp, (ulong)compute_sum(rsdp, sizeof(*rsdp)));
        return false;
    }

    stdlib::mem_cpy(oem_id_, rsdp->first_part.oemid, sizeof(rsdp->first_part.oemid));
    oem_id_[sizeof(rsdp->first_part.oemid)] = '\0';

    /* ACPI 2.0 deprecated the RSDT: its entries are 32-bit, so it cannot
       name a table above 4 GiB, and firmware is free to leave rsdt_address
       at zero once it supplies an XSDT. Prefer the XSDT whenever the RSDP
       is new enough to have one. The EX44's AMI firmware happens to supply
       both -- Linux uses the XSDT there and this kernel used to use the
       RSDT, which worked only because that firmware is generous. */
    if (rsdp->first_part.revision >= 2 && rsdp->xsdt_address != 0)
    {
        root_phys_addr = (ulong)rsdp->xsdt_address;
        is_xsdt = true;
    }
    else
    {
        root_phys_addr = rsdp->first_part.rsdt_address;
        is_xsdt = false;
    }

    klog(0, "Rsdp 0x%p revision %lu oem_id_ %s %s 0x%lX",
        rsdp, (ulong)rsdp->first_part.revision, oem_id_,
        is_xsdt ? "Xsdt" : "Rsdt", root_phys_addr);

    return root_phys_addr != 0;
}

/* Scan a physical range (16-byte aligned slots) for the RSDP signature.
   Only used for the legacy BIOS areas (EBDA, 0xE0000-0xFFFFF); reading
   arbitrary reserved regions is unsafe on real hardware (MMIO). */
bool Acpi::scan_rsdp_range(ulong phy_start, ulong phy_end, ulong& root_phys_addr,
    bool& is_xsdt)
{
    auto& pt = kernel::mm::PageTable::get_instance();

    ulong page_start = stdlib::round_down(phy_start, consts::PAGE_SIZE);
    for (ulong curr = page_start; curr < phy_end; curr += consts::PAGE_SIZE)
    {
        ulong page_va = pt.tmp_map_page(curr);
        if (!page_va)
        {
            klog(0, "Can't map 0x%lX", curr);
            return false;
        }

        ulong scan_start = (curr < phy_start) ? page_va + (phy_start - curr) : page_va;
        ulong scan_end = ((curr + consts::PAGE_SIZE) > phy_end)
            ? page_va + (phy_end - curr) : page_va + consts::PAGE_SIZE;

        for (ulong va = scan_start; va + sizeof(RSDPDescriptor) <= scan_end; va += 16)
        {
            RSDPDescriptor20 *rsdp = reinterpret_cast<RSDPDescriptor20*>(va);
            if (rsdp->first_part.signature == RSDP_SIGNATURE)
            {
                klog(0, "Checking rsdp va 0x%p pha 0x%lX", rsdp, curr + (va - page_va));
                /* parse_rsdp reads the full 36-byte RSDPDescriptor20 when
                   Revision >= 2; the loop bound only guarantees the 20-byte
                   first_part, so skip a 2.0 RSDP whose extended fields would
                   read past this single mapped page. */
                if (rsdp->first_part.revision >= 2 &&
                    va + sizeof(RSDPDescriptor20) > scan_end)
                    continue;
                if (parse_rsdp(rsdp, root_phys_addr, is_xsdt))
                {
                    pt.tmp_unmap_page(page_va);
                    return true;
                }
            }
        }
        pt.tmp_unmap_page(page_va);
    }

    return false;
}

bool Acpi::find_root_table(ulong& root_phys_addr, bool& is_xsdt)
{
    /* 1. RSDP copy from the Multiboot2 ACPI tag. On UEFI this is the
       only way to find it: the RSDP lives in ACPI-reclaimable memory,
       not in the legacy BIOS area. */
    size_t grub_rsdp_size = 0;
    const void* grub_rsdp = grub::get_acpi_rsdp(grub_rsdp_size);
    if (grub_rsdp != nullptr && grub_rsdp_size >= sizeof(RSDPDescriptor))
    {
        RSDPDescriptor20 copy;
        stdlib::mem_set(&copy, 0, sizeof(copy));
        stdlib::mem_cpy(&copy, grub_rsdp,
            (grub_rsdp_size < sizeof(copy)) ? grub_rsdp_size : sizeof(copy));

        if (copy.first_part.signature == RSDP_SIGNATURE &&
            parse_rsdp(&copy, root_phys_addr, is_xsdt))
        {
            klog(0, "Rsdp from multiboot tag, %s 0x%lX",
                is_xsdt ? "Xsdt" : "Rsdt", root_phys_addr);
            return true;
        }

        klog(0, "Multiboot rsdp tag invalid");
    }

    auto& pt = kernel::mm::PageTable::get_instance();

    /* 2. First KB of the EBDA; its segment is at BDA 0x40E */
    ulong bda_va = pt.tmp_map_page(0);
    if (bda_va != 0)
    {
        ulong ebda = ((ulong)*(u16*)(bda_va + 0x40E)) << 4;
        pt.tmp_unmap_page(bda_va);

        if (ebda >= 0x80000 && ebda < 0xA0000)
        {
            if (scan_rsdp_range(ebda, ebda + 1024, root_phys_addr, is_xsdt))
                return true;
        }
    }

    /* 3. BIOS read-only area 0xE0000-0xFFFFF */
    if (scan_rsdp_range(0xE0000, 0x100000, root_phys_addr, is_xsdt))
        return true;

    klog(0, "Rsdp not found");
    return false;
}

stdlib::Error Acpi::parse_root_table(ACPISDTHeader* root)
{
    const char* signature = root_is_xsdt_ ? "XSDT" : "RSDT";

    if (stdlib::strn_cmp(root->signature, signature, sizeof(root->signature)) != 0)
    {
        klog(ACPI_LL, "%s 0x%p invalid signature", signature, root);
        return make_error(stdlib::Error::NOT_FOUND);
    }

    if (CHECK_RSDT_CHECKSUM)
    {
        if (compute_sum(root, root_length_) != 0)
        {
            klog(ACPI_LL, "%s 0x%p checksum failed 0x%lX vs 0x%lX", signature,
                root, (ulong)compute_sum(root, root_length_), (ulong)root->checksum);
             return make_error(stdlib::Error::NOT_FOUND);
        }
    }

    return make_error(stdlib::Error::SUCCESS);
}

ulong Acpi::root_entry(size_t index)
{
    const u8* base = reinterpret_cast<const u8*>(root_) +
        OFFSET_OF(ACPISDTHeader, entry);

    if (root_is_xsdt_)
    {
        u64 value;
        stdlib::mem_cpy(&value, base + index * sizeof(u64), sizeof(value));
        return (ulong)value;
    }

    u32 value;
    stdlib::mem_cpy(&value, base + index * sizeof(u32), sizeof(value));
    return (ulong)value;
}

namespace
{

/* What this kernel reads of ACPI: the MADT, the FADT, the HPET table, the
   WDAT, the DMAR and the MCFG (Acpi::table_ has one slot for each) */
const char* const WANTED_TABLES[] = { "APIC", "FACP", "HPET", "WDAT", "DMAR", "MCFG", "SRAT", "SLIT" };

/* A little-endian field of `bytes` bytes (8 at most) at p, a byte at a time:
   the DMAR's and the MCFG's structures are packed, and only as aligned as
   the table, which is 4-byte aligned if that */
u64 read_le(const u8* p, size_t bytes)
{
    u64 value = 0;
    for (size_t i = 0; i < bytes; i++)
        value |= static_cast<u64>(p[i]) << (8 * i);
    return value;
}

/* An ECAM window decodes a MiB a bus (PCI Express 7.2.2) */
const ulong ECAM_BUS_SHIFT = 20;

}

long Acpi::wanted_index(const char* signature)
{
    static_assert(sizeof(WANTED_TABLES) / sizeof(WANTED_TABLES[0]) == MAX_TABLES, "a slot of Table for each table read");

    for (size_t i = 0; i < stdlib::array_size(WANTED_TABLES); i++)
    {
        if (stdlib::strn_cmp(signature, WANTED_TABLES[i], 4) == 0)
            return (long)i;
    }

    return -1;
}

void Acpi::unmap_table_range(void* va, ulong len)
{
    auto& pt = mm::PageTable::get_instance();

    ulong start = reinterpret_cast<ulong>(va) & ~(consts::PAGE_SIZE - 1);
    ulong end = reinterpret_cast<ulong>(va) + len;
    for (ulong page = start; page < end; page += consts::PAGE_SIZE)
        pt.tmp_unmap_page(page);
}

Acpi::ACPISDTHeader* Acpi::map_header(ulong phys)
{
    if (phys > mm::MemoryMap::MAX_PHYS_ADDR - sizeof(ACPISDTHeader))
    {
        klog(0, "Acpi: table at 0x%lX, past the physical address space", phys);
        return nullptr;
    }

    auto* header = reinterpret_cast<ACPISDTHeader*>(
        mm::PageTable::get_instance().tmp_map_range(phys, sizeof(ACPISDTHeader)));
    if (header == nullptr)
        klog(0, "Acpi: can't map table at 0x%lX", phys);
    return header;
}

Acpi::ACPISDTHeader* Acpi::map_whole(ACPISDTHeader* header, ulong phys, u32& length)
{
    length = header->length;
    char signature[5];
    stdlib::mem_cpy(signature, header->signature, sizeof(header->signature));
    signature[4] = '\0';

    if (length < sizeof(ACPISDTHeader) || length > MAX_TABLE_LENGTH ||
        length > mm::MemoryMap::MAX_PHYS_ADDR - phys)
    {
        klog(0, "Acpi: %s at 0x%lX length %lu, not a table to map", signature, phys, (ulong)length);
        unmap_table_range(header, sizeof(ACPISDTHeader));
        return nullptr;
    }

    /* Mapped again, as long as it says it is: what a parser reads of it is
       that range, and the fuzzer holds the parsers to it byte for byte */
    unmap_table_range(header, sizeof(ACPISDTHeader));
    auto* table = reinterpret_cast<ACPISDTHeader*>(mm::PageTable::get_instance().tmp_map_range(phys, length));
    if (table == nullptr)
        klog(0, "Acpi: can't map %s at 0x%lX length %lu", signature, phys, (ulong)length);
    return table;
}

void Acpi::release_tables()
{
    for (size_t i = 0; i < stdlib::array_size(table_); i++)
    {
        if (table_[i] != nullptr)
        {
            unmap_table_range(table_[i], table_length_[i]);
            table_[i] = nullptr;
            table_length_[i] = 0;
        }
    }

    if (root_ != nullptr)
    {
        unmap_table_range(root_, root_length_);
        root_ = nullptr;
        root_length_ = 0;
    }
}

Acpi::ACPISDTHeader* Acpi::lookup_table(const char *name, u32& length)
{
    length = 0;
    if (stdlib::str_len(name) != 4)
    {
        return nullptr;
    }

    long index = wanted_index(name);
    if (index < 0)
        return nullptr;

    length = table_length_[index];
    return table_[index];
}

stdlib::Error Acpi::parse_table_pointers()
{
    if (root_length_ <= sizeof(ACPISDTHeader))
        return make_error(stdlib::Error::NOT_FOUND);

    const size_t entry_size = root_is_xsdt_ ? sizeof(u64) : sizeof(u32);
    size_t table_count = (root_length_ - OFFSET_OF(ACPISDTHeader, entry)) / entry_size;
    klog(0, "Acpi: %s, %lu tables", root_is_xsdt_ ? "Xsdt" : "Rsdt", table_count);

    for (size_t i = 0; i < table_count; i++)
    {
        /* The header first, for the signature: a table this kernel reads is
           then mapped whole, so that its parser has it at one VA range */
        ulong entry_phys = root_entry(i);
        ACPISDTHeader* header = map_header(entry_phys);
        if (header == nullptr)
            continue;

        char table_signature[5];
        stdlib::mem_cpy(table_signature, header->signature, sizeof(header->signature));
        table_signature[4] = '\0';

        /* A table this kernel does not read, or a second of one it does, is
           let go of here: neither is worth a slot of the window */
        long wanted = wanted_index(table_signature);
        if (wanted < 0 || table_[wanted] != nullptr)
        {
            klog(ACPI_LL, "Acpi: table %lu %s len %lu not kept", (ulong)i, table_signature,
                (ulong)header->length);
            unmap_table_range(header, sizeof(ACPISDTHeader));
            continue;
        }

        u32 table_length = 0;
        ACPISDTHeader* table = map_whole(header, entry_phys, table_length);
        if (table == nullptr)
            continue;

        klog(ACPI_LL, "Acpi: table 0x%p %s len %lu", table, table_signature, (ulong)table_length);

        table_[wanted] = table;
        table_length_[wanted] = table_length;
    }

    return make_error(stdlib::Error::SUCCESS);
}

bool Acpi::registers_in_page(ulong phys, ulong bytes)
{
    return (phys & (consts::PAGE_SIZE - 1)) <= consts::PAGE_SIZE - bytes;
}

stdlib::Error Acpi::parse_madt()
{
    u32 length = 0;
    ACPISDTHeader* sdt_header = lookup_table("APIC", length);
    if (sdt_header == nullptr)
    {
        return make_error(stdlib::Error::NOT_FOUND);
    }

    klog(ACPI_LL, "Acpi: MADT 0x%p", sdt_header);

    if (length < sizeof(ACPISDTHeader) + sizeof(MadtHeader))
    {
        klog(0, "Acpi: MADT too short: %lu", (ulong)length);
        return make_error(stdlib::Error::INVALID_VALUE);
    }

    MadtHeader* header = reinterpret_cast<MadtHeader*>(sdt_header + 1);
    klog(ACPI_LL, "Acpi: MADT LIntCtrl 0x%lX flags 0x%lX",
        (ulong)header->local_int_ctrl_address, (ulong)header->flags);

    const ulong lapic_phys = header->local_int_ctrl_address;
    if (!registers_in_page(lapic_phys, LAPIC_REGISTER_BYTES))
    {
        klog(0, "Acpi: MADT local APIC at 0x%lX, not a page", lapic_phys);
        return make_error(stdlib::Error::INVALID_VALUE);
    }

    lapic_address_ = (void *)mm::PageTable::get_instance().tmp_map_address(lapic_phys);
    if (lapic_address_ == nullptr)
    {
        return make_error(stdlib::Error::NO_MEMORY);
    }

    MadtEntry* entry = &header->entry[0];
    void* madt_end = stdlib::mem_add(sdt_header, length);

    /* Check the 2-byte entry header is within the table before reading
       entry->Length, then that the whole entry fits -- a truncated or corrupt
       MADT would otherwise read Length past the end of the mapped table. */
    while (stdlib::mem_add(entry, sizeof(MadtEntry)) <= madt_end &&
           stdlib::mem_add(entry, entry->length) <= madt_end)
    {
        klog(ACPI_LL, "Acpi: MADT entry 0x%p type %lu len %lu",
            entry, (ulong)entry->type, (ulong)entry->length);

        if (entry->length == 0)
        {
            break;
        }

        switch (entry->type)
        {
        case MADT_ENTRY_TYPE_LAPIC:
        {
            if (entry->length < sizeof(MadtLapicEntry) + sizeof(*entry))
                return make_error(stdlib::Error::INVALID_VALUE);
            MadtLapicEntry* lapic_entry = reinterpret_cast<MadtLapicEntry*>(entry + 1);

            klog(ACPI_LL, "Acpi: MADT lapic procId %lu apic_id %lu flags 0x%lX",
                (ulong)lapic_entry->acpi_process_id, (ulong)lapic_entry->apic_id, (ulong)lapic_entry->flags);

            if (lapic_entry->flags & 0x1)
            {
                if (!CpuTable::get_instance().insert_cpu(lapic_entry->apic_id))
                {
                    klog(ACPI_LL, "Acpi: MADT lapic apic_id %lu ignored (max %lu)",
                        (ulong)lapic_entry->apic_id, (ulong)MAX_CPUS);
                }
            }
            break;
        }
        case MADT_ENTRY_TYPE_IO_APIC:
        {
            if (entry->length < sizeof(MadtIoApicEntry) + sizeof(*entry))
                return make_error(stdlib::Error::INVALID_VALUE);
            MadtIoApicEntry* io_apic_entry = reinterpret_cast<MadtIoApicEntry*>(entry + 1);

            klog(ACPI_LL, "Acpi: MADT ioApicId %lu addr 0x%lX gsi 0x%lX",
                (ulong)io_apic_entry->io_apic_id, (ulong)io_apic_entry->io_apic_address,
                (ulong)io_apic_entry->global_system_interrupt_base);

            /* The IO-APIC driver takes a GSI for the index of its pin, so the
               one it drives is the one whose pins start at GSI 0 -- the
               legacy IRQs' -- or, on a machine with none such, the first.
               A machine may have several (AMD's FCH and GNB): only the one
               kept is mapped. */
            const u32 base = io_apic_entry->global_system_interrupt_base;
            const ulong io_apic_phys = io_apic_entry->io_apic_address;
            if (!registers_in_page(io_apic_phys, IO_APIC_REGISTER_BYTES))
            {
                klog(0, "Acpi: MADT IO-APIC at 0x%lX, its registers across a page, ignored", io_apic_phys);
                break;
            }
            if (io_apic_address_ != nullptr && (io_apic_gsi_base_ == 0 || base != 0))
                break;

            void* mapped = (void *)mm::PageTable::get_instance().tmp_map_address(io_apic_phys);
            if (mapped == nullptr)
            {
                return make_error(stdlib::Error::NO_MEMORY);
            }

            if (io_apic_address_ != nullptr)
                unmap_table_range(io_apic_address_, 1);
            io_apic_address_ = mapped;
            io_apic_gsi_base_ = base;
            io_apic_id_ = io_apic_entry->io_apic_id;
            break;
        }
        case MADT_ENTRY_TYPE_INT_SRC_OVERRIDE:
        {
            if (entry->length < sizeof(MadtIntSrcOverrideEntry) + sizeof(*entry))
                return make_error(stdlib::Error::INVALID_VALUE);
            MadtIntSrcOverrideEntry* iso_entry = reinterpret_cast<MadtIntSrcOverrideEntry*>(entry + 1);

            klog(ACPI_LL, "Acpi: MADT bus 0x%lX irq 0x%lX gsi 0x%lX flags 0x%lX",
                (ulong)iso_entry->bus_source, (ulong)iso_entry->irq_source, (ulong)iso_entry->global_system_interrupt,
                (ulong)iso_entry->flags);

            register_irq_to_gsi(iso_entry->irq_source, iso_entry->global_system_interrupt, iso_entry->flags);
            break;
        }
        default:
            break;
        }

        entry = static_cast<MadtEntry*>(stdlib::mem_add(entry, entry->length));
    }

    /* The interrupts are the IO-APIC's to route: without one to drive, the
       boot would fault in its driver at address 0 */
    if (io_apic_address_ == nullptr)
    {
        klog(0, "Acpi: MADT names no IO-APIC");
        return make_error(stdlib::Error::NOT_FOUND);
    }

    return make_error(stdlib::Error::SUCCESS);
}

void Acpi::parse_fadt()
{
    u32 length = 0;
    ACPISDTHeader* sdt_header = lookup_table("FACP", length);
    if (sdt_header == nullptr)
    {
        klog(ACPI_LL, "Acpi: no FADT table");
        return;
    }

    ulong body_len = length - sizeof(ACPISDTHeader);
    FadtFields* fadt = reinterpret_cast<FadtFields*>(sdt_header + 1);

    /* pm1a_cnt_blk sits at body offset +28; need at least 32 bytes of body */
    static const ulong PM1A_CNT_BLK_END = OFFSET_OF(FadtFields, pm1a_cnt_blk) + sizeof(fadt->pm1a_cnt_blk);
    if (body_len >= PM1A_CNT_BLK_END)
    {
        pm1a_cnt_port_ = fadt->pm1a_cnt_blk;
        klog(ACPI_LL, "Acpi: FADT PM1a_CNT port 0x%lX", pm1a_cnt_port_);
    }

    static const ulong PM1B_CNT_BLK_END = OFFSET_OF(FadtFields, pm1b_cnt_blk) + sizeof(fadt->pm1b_cnt_blk);
    if (body_len >= PM1B_CNT_BLK_END)
        pm1b_cnt_port_ = fadt->pm1b_cnt_blk;

    /* The DSDT: X_DSDT where the table is long enough to have the field and
       it is not zero (ACPI 2.0 and later, and the one ACPICA takes of the
       two), else the 32-bit DSDT. It is read by parse_dsdt, once this table
       is let go of. */
    static const ulong DSDT_END = OFFSET_OF(FadtFields, dsdt) + sizeof(fadt->dsdt);
    static const ulong X_DSDT_END = OFFSET_OF(FadtFields, x_dsdt) + sizeof(fadt->x_dsdt);
    if (body_len >= DSDT_END)
        dsdt_phys_ = fadt->dsdt;
    if (body_len >= X_DSDT_END && fadt->x_dsdt != 0)
        dsdt_phys_ = (ulong)fadt->x_dsdt;

    /* Flags + reset_reg + reset_value require at least 93 bytes of body (ACPI 2.0+) */
    static const ulong RESET_VALUE_END = OFFSET_OF(FadtFields, reset_value) + sizeof(fadt->reset_value);
    if (body_len >= RESET_VALUE_END)
    {
        klog(ACPI_LL, "Acpi: FADT flags 0x%lX", (ulong)fadt->flags);

        /* RESET_REG_SUP is bit 10 of Flags */
        static const u32 RESET_REG_SUP = (1u << 10);
        if ((fadt->flags & RESET_REG_SUP) && fadt->reset_reg.address_space_id == 1 /* I/O */)
        {
            reset_reg_valid_ = true;
            reset_reg_port_ = (ulong)fadt->reset_reg.address;
            reset_val_ = fadt->reset_value;
            klog(ACPI_LL, "Acpi: FADT RESET_REG port 0x%lX value 0x%lX",
                reset_reg_port_, (ulong)reset_val_);
        }
    }

    /* Century CMOS register selector (ACPI 2.0+, body offset +72). 0 means the
       platform has no century register, so the RTC year must not trust it. */
    static const ulong CENTURY_END = OFFSET_OF(FadtFields, century) + sizeof(fadt->century);
    if (body_len >= CENTURY_END)
    {
        century_register_ = fadt->century;
        klog(ACPI_LL, "Acpi: FADT century register 0x%lX", (ulong)century_register_);
    }
}

u8 Acpi::get_century_register()
{
    return century_register_;
}

bool Acpi::parse_s5_type(const u8* aml, size_t limit, size_t& at, u8& value)
{
    if (at >= limit)
        return false;

    u8 type;
    const u8 op = aml[at++];
    if (op == AML_ZERO_OP)
    {
        type = 0;
    }
    else if (op == AML_ONE_OP)
    {
        type = 1;
    }
    else if (op == AML_BYTE_PREFIX)
    {
        if (at >= limit)
            return false;
        type = aml[at++];
    }
    else
    {
        return false;
    }

    if (type > SLP_TYP_MAX)
        return false;

    value = type;
    return true;
}

bool Acpi::parse_s5(const u8* aml, size_t len, u8& typ_a, u8& typ_b)
{
    static const u8 NAME[] = { '_', 'S', '5', '_' };

    size_t at = 0;
    if (at >= len || aml[at] != AML_NAME_OP)
        return false;
    at++;

    if (at < len && aml[at] == AML_ROOT_CHAR)
        at++;

    if (len - at < sizeof(NAME))
        return false;
    for (size_t i = 0; i < sizeof(NAME); i++)
    {
        if (aml[at + i] != NAME[i])
            return false;
    }
    at += sizeof(NAME);

    if (at >= len || aml[at] != AML_PACKAGE_OP)
        return false;
    at++;

    /* The package's length is counted from its own first byte */
    if (at >= len)
        return false;
    const size_t pkg_start = at;
    const u8 lead = aml[at];
    const size_t follow = lead >> AML_PKG_FOLLOW_SHIFT;
    if (len - at < 1 + follow)
        return false;
    size_t pkg_len = lead & ((follow == 0) ? AML_PKG_SHORT_MASK : AML_PKG_LOW_MASK);
    for (size_t i = 0; i < follow; i++)
        pkg_len |= (size_t)aml[at + 1 + i] << (AML_PKG_LOW_BITS + 8 * i);
    at += 1 + follow;

    /* What is read of the package is inside it, and inside what is there */
    const size_t limit = stdlib::min(len, pkg_start + pkg_len);
    if (at >= limit)
        return false;
    const u8 elements = aml[at++];
    if (elements < 2)
        return false;

    u8 a = 0;
    u8 b = 0;
    if (!parse_s5_type(aml, limit, at, a) || !parse_s5_type(aml, limit, at, b))
        return false;

    typ_a = a;
    typ_b = b;
    return true;
}

void Acpi::parse_dsdt()
{
    if (dsdt_phys_ == 0)
    {
        klog(0, "Acpi: the FADT names no DSDT, so no \\_S5: no ACPI power-off");
        return;
    }

    const ulong phys = dsdt_phys_;
    ACPISDTHeader* header = map_header(phys);
    if (header == nullptr)
        return;

    const u32 length = header->length;
    const bool is_dsdt = stdlib::strn_cmp(header->signature, "DSDT", sizeof(header->signature)) == 0;
    unmap_table_range(header, sizeof(ACPISDTHeader));
    if (!is_dsdt || length < sizeof(ACPISDTHeader) || length > MAX_DSDT_LENGTH ||
        length > mm::MemoryMap::MAX_PHYS_ADDR - phys)
    {
        klog(0, "Acpi: 0x%lX is %s (length %lu), so no \\_S5: no ACPI power-off", phys,
            is_dsdt ? "a DSDT not to be read" : "no DSDT", (ulong)length);
        return;
    }

    /* A page of it at a time, with as much of the next as a \_S5 begun in
       this one's last byte runs to: two slots of the window at most, each
       mapping given back before the next is taken */
    auto& pt = mm::PageTable::get_instance();
    const ulong end = phys + length;
    for (ulong at = phys + sizeof(ACPISDTHeader); at < end; )
    {
        const ulong page_end = stdlib::min((at & ~(consts::PAGE_SIZE - 1)) + consts::PAGE_SIZE, end);
        const ulong map_end = stdlib::min(page_end + (S5_MAX_BYTES - 1), end);
        const ulong mapped = pt.tmp_map_range(at, map_end - at);
        if (mapped == 0)
        {
            klog(0, "Acpi: can't map the DSDT at 0x%lX, so no \\_S5: no ACPI power-off", at);
            return;
        }

        const u8* aml = reinterpret_cast<const u8*>(mapped);
        bool found = false;
        for (ulong i = 0; i < page_end - at && !found; i++)
        {
            if (aml[i] == AML_NAME_OP)
                found = parse_s5(aml + i, map_end - at - i, s5_typ_a_, s5_typ_b_);
        }
        unmap_table_range(reinterpret_cast<void*>(mapped), map_end - at);

        if (found)
        {
            s5_valid_ = true;
            klog(ACPI_LL, "Acpi: DSDT 0x%lX length %lu, \\_S5 is SLP_TYP %lu and %lu", phys,
                (ulong)length, (ulong)s5_typ_a_, (ulong)s5_typ_b_);
            return;
        }
        at = page_end;
    }

    klog(0, "Acpi: DSDT 0x%lX length %lu holds no \\_S5 to read: no ACPI power-off", phys, (ulong)length);
}

void Acpi::parse_hpet()
{
    u32 length = 0;
    ACPISDTHeader* sdt_header = lookup_table("HPET", length);
    if (sdt_header == nullptr)
    {
        klog(ACPI_LL, "Acpi: no HPET table");
        return;
    }

    if (length < sizeof(ACPISDTHeader) + sizeof(HpetTableBody))
    {
        klog(0, "Acpi: HPET table too short: %lu", (ulong)length);
        return;
    }

    HpetTableBody* hpet = reinterpret_cast<HpetTableBody*>(sdt_header + 1);

    /* base_address must be system memory (address_space_id == 0) */
    if (hpet->base_address.address_space_id != 0)
    {
        klog(0, "Acpi: HPET base not in system memory (id %lu)", (ulong)hpet->base_address.address_space_id);
        return;
    }

    hpet_base_phys_ = (ulong)hpet->base_address.address;
    hpet_min_tick_  = hpet->minimum_clock_tick;

    klog(ACPI_LL, "Acpi: HPET base 0x%lX minTick %lu blockId 0x%lX",
        hpet_base_phys_, (ulong)hpet_min_tick_, (ulong)hpet->event_timer_block_id);
}

/*
 * WDAT (ACPI "Watchdog Action Table") describes a watchdog the firmware
 * hands to the OS as a list of register instructions.  Its presence means
 * the platform expects the OS to drive the watchdog through those
 * instructions rather than through a native driver -- and it usually
 * describes the very same TCO block the tco_wdt driver would grab.  We do
 * not implement the WDAT instruction interpreter, so all we do here is
 * record the fact and let tco_wdt keep its hands off the hardware.
 */
void Acpi::parse_wdat()
{
    u32 length = 0;
    ACPISDTHeader* sdt_header = lookup_table("WDAT", length);
    if (sdt_header == nullptr)
    {
        klog(ACPI_LL, "Acpi: no WDAT table");
        return;
    }

    if (length < sizeof(ACPISDTHeader) + sizeof(WdatTableBody))
    {
        klog(0, "Acpi: WDAT table too short: %lu", (ulong)length);
        return;
    }

    WdatTableBody* wdat = reinterpret_cast<WdatTableBody*>(sdt_header + 1);

    /* Trust the table length over the Entries count */
    size_t max_entries = (length - sizeof(ACPISDTHeader) - sizeof(WdatTableBody))
        / sizeof(WdatEntry);
    size_t entries = wdat->entries;
    if (entries > max_entries)
    {
        klog(0, "Acpi: WDAT claims %lu entries, table holds %lu",
            (ulong)entries, (ulong)max_entries);
        entries = max_entries;
    }

    const WdatEntry* entry = reinterpret_cast<const WdatEntry*>(wdat + 1);
    for (size_t i = 0; i < entries; i++)
    {
        if (entry[i].register_region.address_space_id == GAS_SPACE_SYSTEM_IO &&
            entry[i].register_region.address == RTC_PORT_BASE)
        {
            klog(0, "Acpi: WDAT drives the RTC, ignoring it");
            return;
        }
    }

    firmware_watchdog_ = true;

    klog(0, "Acpi: WDAT present (%lu entries, period %lu ms), firmware owns the watchdog",
        (ulong)entries, (ulong)wdat->timer_period);
}

bool Acpi::parse_dmar_scopes(const u8* scopes, size_t len, DmarScope* scope, size_t& count)
{
    count = 0;
    for (size_t at = 0; at < len; )
    {
        if (len - at < SCOPE_FIXED_BYTES)
        {
            klog(0, "Acpi: DMAR: a device scope cut %lu bytes in", (ulong)(len - at));
            return false;
        }

        const u8* d = scopes + at;
        const size_t d_len = d[1];
        if (d_len < SCOPE_FIXED_BYTES || d_len > len - at || (d_len - SCOPE_FIXED_BYTES) % 2 != 0)
        {
            klog(0, "Acpi: DMAR: a device scope of length %lu, where %lu bytes are left", (ulong)d_len,
                (ulong)(len - at));
            return false;
        }

        const size_t hops = (d_len - SCOPE_FIXED_BYTES) / 2;
        if (hops > MAX_DMAR_PATH || count >= MAX_DMAR_SCOPES)
        {
            klog(0, "Acpi: DMAR: %s", (hops > MAX_DMAR_PATH) ? "a device scope's path longer than kept" :
                "more device scopes than kept");
            return false;
        }

        DmarScope& s = scope[count];
        s.type = d[0];
        s.enumeration_id = d[4];
        s.start_bus = d[5];
        s.path_length = static_cast<u8>(hops);
        for (size_t h = 0; h < MAX_DMAR_PATH; h++)
        {
            s.path[h][0] = (h < hops) ? d[SCOPE_FIXED_BYTES + 2 * h] : 0;
            s.path[h][1] = (h < hops) ? d[SCOPE_FIXED_BYTES + 2 * h + 1] : 0;
        }
        count++;
        at += d_len;
    }
    return true;
}

void Acpi::parse_dmar()
{
    u32 length = 0;
    ACPISDTHeader* sdt_header = lookup_table("DMAR", length);
    if (sdt_header == nullptr)
    {
        klog(ACPI_LL, "Acpi: no DMAR table");
        return;
    }

    if (length < sizeof(ACPISDTHeader) + DMAR_BODY_BYTES)
    {
        klog(0, "Acpi: DMAR table too short: %lu", (ulong)length);
        return;
    }

    /* The width is the host address width field and one: a field of 63 or
       more is 64 bits, which is all an address has */
    static const u8 MAX_WIDTH = 64;
    const u8* table = reinterpret_cast<const u8*>(sdt_header);
    const u8 haw = table[sizeof(ACPISDTHeader)];
    dmar_present_ = true;
    dmar_complete_ = true;
    dmar_width_ = (haw >= MAX_WIDTH - 1) ? MAX_WIDTH : static_cast<u8>(haw + 1);
    dmar_flags_ = table[sizeof(ACPISDTHeader) + 1];

    for (size_t at = sizeof(ACPISDTHeader) + DMAR_BODY_BYTES; at < length; )
    {
        if (length - at < DMAR_STRUCT_HEADER_BYTES)
        {
            klog(0, "Acpi: DMAR: a structure cut by the table at %lu", (ulong)at);
            dmar_complete_ = false;
            break;
        }

        const u8* s = table + at;
        const u16 type = static_cast<u16>(read_le(s, 2));
        const size_t s_len = static_cast<size_t>(read_le(s + 2, 2));
        if (s_len < DMAR_STRUCT_HEADER_BYTES || s_len > length - at)
        {
            klog(0, "Acpi: DMAR: a structure of type %lu at %lu of length %lu, the table %lu long", (ulong)type,
                (ulong)at, (ulong)s_len, (ulong)length);
            dmar_complete_ = false;
            break;
        }
        at += s_len;

        if (type == DMAR_TYPE_DRHD)
        {
            if (s_len < DRHD_FIXED_BYTES || dmar_unit_count_ >= MAX_DMAR_UNITS)
            {
                klog(0, "Acpi: DMAR: %s", (s_len < DRHD_FIXED_BYTES) ? "a unit shorter than its fields" :
                    "more units than kept");
                dmar_complete_ = false;
                continue;
            }
            DmarUnit& unit = dmar_unit_[dmar_unit_count_++];
            unit.include_all = (s[4] & DRHD_INCLUDE_PCI_ALL) != 0;
            unit.size = s[5];
            unit.segment = static_cast<u16>(read_le(s + 6, 2));
            unit.base = static_cast<ulong>(read_le(s + 8, 8));
            if (!parse_dmar_scopes(s + DRHD_FIXED_BYTES, s_len - DRHD_FIXED_BYTES, unit.scope, unit.scope_count))
                dmar_complete_ = false;
            klog(ACPI_LL, "Acpi: DMAR: unit at 0x%lX, segment %lu%s, %lu device scopes", unit.base,
                (ulong)unit.segment, unit.include_all ? ", every device" : "", (ulong)unit.scope_count);
        }
        else if (type == DMAR_TYPE_RMRR)
        {
            if (s_len < RMRR_FIXED_BYTES || dmar_reserved_count_ >= MAX_DMAR_RESERVED)
            {
                klog(0, "Acpi: DMAR: %s", (s_len < RMRR_FIXED_BYTES) ? "a reserved region shorter than its fields" :
                    "more reserved regions than kept");
                dmar_complete_ = false;
                continue;
            }
            DmarReserved& region = dmar_reserved_[dmar_reserved_count_++];
            region.segment = static_cast<u16>(read_le(s + 6, 2));
            region.base = static_cast<ulong>(read_le(s + 8, 8));
            region.limit = static_cast<ulong>(read_le(s + 16, 8));
            if (!parse_dmar_scopes(s + RMRR_FIXED_BYTES, s_len - RMRR_FIXED_BYTES, region.scope, region.scope_count))
                dmar_complete_ = false;
            klog(ACPI_LL, "Acpi: DMAR: reserved 0x%lX-0x%lX, segment %lu, %lu device scopes", region.base,
                region.limit, (ulong)region.segment, (ulong)region.scope_count);
        }
    }

    klog(0, "Acpi: DMAR: %lu-bit DMA, flags 0x%lX, %lu units, %lu reserved regions%s", (ulong)dmar_width_,
        (ulong)dmar_flags_, (ulong)dmar_unit_count_, (ulong)dmar_reserved_count_,
        dmar_complete_ ? "" : "; not all of it kept, so no IOMMU");
}

void Acpi::parse_mcfg()
{
    u32 length = 0;
    ACPISDTHeader* sdt_header = lookup_table("MCFG", length);
    if (sdt_header == nullptr)
    {
        klog(ACPI_LL, "Acpi: no MCFG table");
        return;
    }

    if (length < sizeof(ACPISDTHeader) + MCFG_RESERVED_BYTES)
    {
        klog(0, "Acpi: MCFG table too short: %lu", (ulong)length);
        return;
    }

    /* Segment 0's first window: the only segment the PCI code reaches. Its
       base is bus 0's, whatever its first bus, a MiB a bus aligned; one that
       would run past the physical address space is not a window. */
    const u8* table = reinterpret_cast<const u8*>(sdt_header);
    const size_t entries = (length - sizeof(ACPISDTHeader) - MCFG_RESERVED_BYTES) / MCFG_ENTRY_BYTES;
    for (size_t i = 0; i < entries; i++)
    {
        const u8* e = table + sizeof(ACPISDTHeader) + MCFG_RESERVED_BYTES + i * MCFG_ENTRY_BYTES;
        const u64 base = read_le(e, 8);
        const u16 segment = static_cast<u16>(read_le(e + 8, 2));
        const u8 start = e[10];
        const u8 end = e[11];
        const u64 span = static_cast<u64>(end + 1) << ECAM_BUS_SHIFT;
        if (segment != 0 || base == 0 || start > end || (base & ((1ULL << ECAM_BUS_SHIFT) - 1)) != 0 ||
            base > mm::MemoryMap::MAX_PHYS_ADDR || span > mm::MemoryMap::MAX_PHYS_ADDR - base)
        {
            klog(0, "Acpi: MCFG: window 0x%lX segment %lu buses %lu-%lu passed over", (ulong)base, (ulong)segment,
                (ulong)start, (ulong)end);
            continue;
        }
        mcfg_base_ = static_cast<ulong>(base);
        mcfg_start_bus_ = start;
        mcfg_end_bus_ = end;
        klog(0, "Acpi: MCFG: ECAM at 0x%lX, buses %lu-%lu", mcfg_base_, (ulong)start, (ulong)end);
        return;
    }
    klog(0, "Acpi: MCFG names no ECAM window of segment 0");
}

void Acpi::parse_srat()
{
    u32 length = 0;
    ACPISDTHeader* sdt_header = lookup_table("SRAT", length);
    if (sdt_header == nullptr)
    {
        klog(ACPI_LL, "Acpi: no SRAT table");
        return;
    }

    auto& numa = Numa::get_instance();
    if (length < sizeof(ACPISDTHeader) + SRAT_BODY_BYTES)
    {
        klog(0, "Acpi: SRAT table too short: %lu", (ulong)length);
        numa.refuse("the SRAT shorter than its header");
        return;
    }

    const u8* table = reinterpret_cast<const u8*>(sdt_header);
    const bool wide_domains = sdt_header->revision >= SRAT_WIDE_DOMAIN_REVISION;
    ulong cpus = 0;
    ulong ranges = 0;
    ulong not_kept = 0;

    for (size_t at = sizeof(ACPISDTHeader) + SRAT_BODY_BYTES; at < length; )
    {
        if (length - at < SRAT_STRUCT_HEADER_BYTES)
        {
            klog(0, "Acpi: SRAT: a structure cut by the table at %lu", (ulong)at);
            numa.refuse("the SRAT cut short");
            break;
        }

        const u8* s = table + at;
        const u8 type = s[0];
        const size_t s_len = s[1];
        if (s_len < SRAT_STRUCT_HEADER_BYTES || s_len > length - at)
        {
            klog(0, "Acpi: SRAT: a structure of type %lu at %lu of length %lu, the table %lu long", (ulong)type,
                (ulong)at, (ulong)s_len, (ulong)length);
            numa.refuse("an SRAT structure of no length, or past the table");
            break;
        }
        at += s_len;

        if (type == SRAT_TYPE_LAPIC || type == SRAT_TYPE_X2APIC)
        {
            const bool x2apic = (type == SRAT_TYPE_X2APIC);
            if (s_len < (x2apic ? SRAT_X2APIC_BYTES : SRAT_LAPIC_BYTES))
            {
                klog(0, "Acpi: SRAT: a CPU's entry of %lu bytes", (ulong)s_len);
                numa.refuse("an SRAT CPU entry shorter than its fields");
                continue;
            }
            const u32 flags = static_cast<u32>(read_le(s + (x2apic ? 12 : 4), 4));
            if ((flags & SRAT_ENABLED) == 0)
                continue;

            u32 domain;
            ulong apic_id;
            if (x2apic)
            {
                domain = static_cast<u32>(read_le(s + 4, 4));
                apic_id = static_cast<ulong>(read_le(s + 8, 4));
            }
            else
            {
                domain = s[2];
                if (wide_domains)
                    domain |= static_cast<u32>(read_le(s + 9, 3)) << 8;
                apic_id = s[3];
            }

            klog(ACPI_LL, "Acpi: SRAT: CPU apic id %lu in domain %lu", apic_id, (ulong)domain);
            if (numa.add_cpu(domain, apic_id))
                cpus++;
            else
                not_kept++;
        }
        else if (type == SRAT_TYPE_MEMORY)
        {
            if (s_len < SRAT_MEMORY_BYTES)
            {
                klog(0, "Acpi: SRAT: a memory entry of %lu bytes", (ulong)s_len);
                numa.refuse("an SRAT memory entry shorter than its fields");
                continue;
            }
            const u32 flags = static_cast<u32>(read_le(s + 28, 4));
            if ((flags & SRAT_ENABLED) == 0)
                continue;

            const u32 domain = static_cast<u32>(read_le(s + 2, 4));
            const ulong base = static_cast<ulong>(read_le(s + 8, 8));
            const ulong len = static_cast<ulong>(read_le(s + 16, 8));
            klog(ACPI_LL, "Acpi: SRAT: memory 0x%lX, 0x%lX bytes, in domain %lu", base, len, (ulong)domain);
            if (numa.add_memory(domain, base, len))
                ranges++;
            else
                not_kept++;
        }
    }

    klog(0, "Acpi: SRAT: %lu CPUs and %lu memory ranges in %lu proximity domains%s", cpus, ranges,
        numa.get_domain_count(), not_kept != 0 ? ", some entries not kept" : "");
}

void Acpi::parse_slit()
{
    u32 length = 0;
    ACPISDTHeader* sdt_header = lookup_table("SLIT", length);
    if (sdt_header == nullptr)
    {
        klog(ACPI_LL, "Acpi: no SLIT table");
        return;
    }

    if (length < sizeof(ACPISDTHeader) + SLIT_BODY_BYTES)
    {
        klog(0, "Acpi: SLIT table too short: %lu", (ulong)length);
        return;
    }

    /* The matrix whole in the table, and a count whose square cannot wrap:
       the table is MAX_TABLE_LENGTH long at the most */
    const u8* table = reinterpret_cast<const u8*>(sdt_header);
    const u64 localities = read_le(table + sizeof(ACPISDTHeader), 8);
    const u64 room = length - sizeof(ACPISDTHeader) - SLIT_BODY_BYTES;
    if (localities == 0 || localities > room || localities * localities > room)
    {
        klog(0, "Acpi: SLIT of %lu localities in %lu bytes: no distances from it", (ulong)localities,
            (ulong)length);
        return;
    }

    /* A domain the SRAT named that the matrix has no row for: the distances
       are the defaults, for all of them */
    auto& numa = Numa::get_instance();
    const ulong domains = numa.get_domain_count();
    for (ulong i = 0; i < domains; i++)
    {
        if (numa.get_domain(i) >= localities)
        {
            klog(0, "Acpi: SLIT of %lu localities, and domain %lu: no distances from it", (ulong)localities,
                (ulong)numa.get_domain(i));
            return;
        }
    }

    const u8* matrix = table + sizeof(ACPISDTHeader) + SLIT_BODY_BYTES;
    for (ulong i = 0; i < domains; i++)
    {
        for (ulong j = 0; j < domains; j++)
        {
            const u64 from = numa.get_domain(i);
            const u64 to = numa.get_domain(j);
            numa.set_distance(i, j, matrix[from * localities + to]);
        }
    }
    klog(0, "Acpi: SLIT: %lu localities", (ulong)localities);
}

stdlib::Error Acpi::parse()
{
    stdlib::Error err = parse_tables();

    /* The tables are read once, here: what is kept of them is the values
       taken out, and on a failure not even the APICs' pages, which nothing
       is then to use */
    release_tables();
    if (!err.ok())
    {
        if (lapic_address_ != nullptr)
        {
            unmap_table_range(lapic_address_, 1);
            lapic_address_ = nullptr;
        }
        if (io_apic_address_ != nullptr)
        {
            unmap_table_range(io_apic_address_, 1);
            io_apic_address_ = nullptr;
            io_apic_gsi_base_ = ~0U;
            io_apic_id_ = 0;
        }
        return err;
    }

    /* The DSDT now, the tables' slots of the window free again */
    parse_dsdt();
    return err;
}

stdlib::Error Acpi::parse_tables()
{
    stdlib::Error err;
    ulong root_phys_addr = 0;
    if (!find_root_table(root_phys_addr, root_is_xsdt_))
    {
        return make_error(stdlib::Error::NOT_FOUND);
    }

    ACPISDTHeader* header = map_header(root_phys_addr);
    if (header == nullptr)
        return make_error(stdlib::Error::NO_MEMORY);

    root_ = map_whole(header, root_phys_addr, root_length_);
    if (root_ == nullptr)
        return make_error(stdlib::Error::INVALID_VALUE);

    err = parse_root_table(root_);
    if (!err.ok())
    {
        return err;
    }

    err = parse_table_pointers();
    if (!err.ok())
    {
        return err;
    }

    err = parse_madt();
    if (!err.ok())
    {
        return err;
    }

    parse_fadt();
    parse_hpet();

    parse_wdat();

    parse_dmar();
    parse_mcfg();

    /* The SRAT before the SLIT: the distances are of the domains it named */
    parse_srat();
    parse_slit();

    return make_error(stdlib::Error::SUCCESS);
}


const char* Acpi::get_oem_id()
{
    return oem_id_;
}

void* Acpi::get_lapic_address()
{
    return lapic_address_;
}

void* Acpi::get_io_apic_address()
{
    return io_apic_address_;
}

u8 Acpi::get_io_apic_id()
{
    return io_apic_id_;
}

void Acpi::register_irq_to_gsi(u8 irq, u32 gsi, u16 flags)
{
    /* Interrupt::register_handler takes a GSI in an u8 */
    static const u32 MAX_GSI = 0xFF;

    for (size_t i = 0; i < irq_to_gsi_size_; i++)
    {
        if (irq_to_gsi_[i].irq == irq)
        {
            klog(0, "Acpi: second override of irq %lu (to gsi %lu) ignored", (ulong)irq, (ulong)gsi);
            return;
        }
    }

    if (gsi > MAX_GSI || irq_to_gsi_size_ >= stdlib::array_size(irq_to_gsi_))
    {
        klog(0, "Acpi: override of irq %lu to gsi %lu ignored: %s", (ulong)irq, (ulong)gsi,
            (gsi > MAX_GSI) ? "past the GSIs taken" : "no room");
        return;
    }

    auto& entry = irq_to_gsi_[irq_to_gsi_size_];
    entry.irq = irq;
    entry.gsi = gsi;
    entry.flags = flags;
    irq_to_gsi_size_++;
}

u16 Acpi::get_irq_flags(u8 irq)
{
    for (size_t i = 0; i < irq_to_gsi_size_; i++)
    {
        auto& entry = irq_to_gsi_[i];
        if (entry.irq == irq)
            return entry.flags;
    }
    return 0; /* Default: conforms to bus specification */
}

u32 Acpi::get_gsi_by_irq(u8 irq)
{
    for (size_t i = 0; i < irq_to_gsi_size_; i++)
    {
        auto& entry = irq_to_gsi_[i];
        if (entry.irq == irq)
        {
            return entry.gsi;
        }
    }

    return irq;
}

ulong Acpi::get_pm1a_cnt_port()
{
    return pm1a_cnt_port_;
}

ulong Acpi::get_pm1b_cnt_port()
{
    return pm1b_cnt_port_;
}

bool Acpi::get_s5(u8& typ_a, u8& typ_b)
{
    if (!s5_valid_)
        return false;

    typ_a = s5_typ_a_;
    typ_b = s5_typ_b_;
    return true;
}

bool Acpi::has_reset_reg()
{
    return reset_reg_valid_;
}

ulong Acpi::get_reset_reg_port()
{
    return reset_reg_port_;
}

u8 Acpi::get_reset_value()
{
    return reset_val_;
}

ulong Acpi::get_hpet_base_phys()
{
    return hpet_base_phys_;
}

u16 Acpi::get_hpet_min_tick()
{
    return hpet_min_tick_;
}

bool Acpi::has_firmware_watchdog()
{
    return firmware_watchdog_;
}

bool Acpi::has_dmar()
{
    return dmar_present_;
}

bool Acpi::dmar_complete()
{
    return dmar_present_ && dmar_complete_;
}

u8 Acpi::get_dmar_width()
{
    return dmar_width_;
}

u8 Acpi::get_dmar_flags()
{
    return dmar_flags_;
}

size_t Acpi::get_dmar_unit_count()
{
    return dmar_unit_count_;
}

const Acpi::DmarUnit* Acpi::get_dmar_unit(size_t index)
{
    return (index < dmar_unit_count_) ? &dmar_unit_[index] : nullptr;
}

size_t Acpi::get_dmar_reserved_count()
{
    return dmar_reserved_count_;
}

const Acpi::DmarReserved* Acpi::get_dmar_reserved(size_t index)
{
    return (index < dmar_reserved_count_) ? &dmar_reserved_[index] : nullptr;
}

ulong Acpi::get_mcfg_base()
{
    return mcfg_base_;
}

u8 Acpi::get_mcfg_start_bus()
{
    return mcfg_start_bus_;
}

u8 Acpi::get_mcfg_end_bus()
{
    return mcfg_end_bus_;
}

}