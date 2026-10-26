#include "acpi.h"

#include <kernel/trace.h>
#include <kernel/cpu.h>
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
    , pm1a_cnt_port_(0)
    , reset_reg_valid_(false)
    , reset_reg_port_(0)
    , reset_val_(0)
    , century_register_(0)
    , hpet_base_phys_(0)
    , hpet_min_tick_(0)
    , firmware_watchdog_(false)
{
    oem_id_[0] = '\0';
    for (size_t i = 0; i < stdlib::array_size(table_); i++)
    {
        table_[i] = nullptr;
        table_length_[i] = 0;
    }
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
        trace(0, "Rsdp 0x%p checksum failed (sum 0x%lX)",
            rsdp, (ulong)compute_sum(rsdp, sizeof(rsdp->first_part)));
        return false;
    }

    /* ACPI 2.0+: the extended checksum covers the whole descriptor */
    if (rsdp->first_part.revision >= 2 &&
        compute_sum(rsdp, sizeof(*rsdp)) != 0)
    {
        trace(0, "Rsdp 0x%p extended checksum failed (sum 0x%lX)",
            rsdp, (ulong)compute_sum(rsdp, sizeof(*rsdp)));
        return false;
    }

    stdlib::mem_cpy(oem_id_, rsdp->first_part.oemid, sizeof(rsdp->first_part.oemid));
    oem_id_[sizeof(rsdp->first_part.oemid)] = '\0';

    /* ACPI 2.0 deprecated the RSDT: its entries are 32-bit, so it cannot
       name a table above 4 GiB, and firmware is free to leave RsdtAddress
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

    trace(0, "Rsdp 0x%p revision %lu OemId %s %s 0x%lX",
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
            trace(0, "Can't map 0x%lX", curr);
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
                trace(0, "Checking rsdp va 0x%p pha 0x%lX", rsdp, curr + (va - page_va));
                /* ParseRsdp reads the full 36-byte RSDPDescriptor20 when
                   Revision >= 2; the loop bound only guarantees the 20-byte
                   FirstPart, so skip a 2.0 RSDP whose extended fields would
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
            trace(0, "Rsdp from multiboot tag, %s 0x%lX",
                is_xsdt ? "Xsdt" : "Rsdt", root_phys_addr);
            return true;
        }

        trace(0, "Multiboot rsdp tag invalid");
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

    trace(0, "Rsdp not found");
    return false;
}

stdlib::Error Acpi::parse_root_table(ACPISDTHeader* root)
{
    const char* signature = root_is_xsdt_ ? "XSDT" : "RSDT";

    if (stdlib::strn_cmp(root->signature, signature, sizeof(root->signature)) != 0)
    {
        trace(ACPI_LL, "%s 0x%p invalid signature", signature, root);
        return make_error(stdlib::Error::NOT_FOUND);
    }

    if (CHECK_RSDT_CHECKSUM)
    {
        if (compute_sum(root, root_length_) != 0)
        {
            trace(ACPI_LL, "%s 0x%p checksum failed 0x%lX vs 0x%lX", signature,
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

/* What this kernel reads of ACPI: the MADT, the FADT, the HPET table and
   the WDAT (Acpi::Table has one slot for each) */
const char* const WANTED_TABLES[] = { "APIC", "FACP", "HPET", "WDAT" };

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
        trace(0, "Acpi: table at 0x%lX, past the physical address space", phys);
        return nullptr;
    }

    auto* header = reinterpret_cast<ACPISDTHeader*>(
        mm::PageTable::get_instance().tmp_map_range(phys, sizeof(ACPISDTHeader)));
    if (header == nullptr)
        trace(0, "Acpi: can't map table at 0x%lX", phys);
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
        trace(0, "Acpi: %s at 0x%lX length %lu, not a table to map", signature, phys, (ulong)length);
        unmap_table_range(header, sizeof(ACPISDTHeader));
        return nullptr;
    }

    /* Mapped again, as long as it says it is: what a parser reads of it is
       that range, and the fuzzer holds the parsers to it byte for byte */
    unmap_table_range(header, sizeof(ACPISDTHeader));
    auto* table = reinterpret_cast<ACPISDTHeader*>(mm::PageTable::get_instance().tmp_map_range(phys, length));
    if (table == nullptr)
        trace(0, "Acpi: can't map %s at 0x%lX length %lu", signature, phys, (ulong)length);
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
    trace(0, "Acpi: %s, %lu tables", root_is_xsdt_ ? "Xsdt" : "Rsdt", table_count);

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
            trace(ACPI_LL, "Acpi: table %lu %s len %lu not kept", (ulong)i, table_signature,
                (ulong)header->length);
            unmap_table_range(header, sizeof(ACPISDTHeader));
            continue;
        }

        u32 table_length = 0;
        ACPISDTHeader* table = map_whole(header, entry_phys, table_length);
        if (table == nullptr)
            continue;

        trace(ACPI_LL, "Acpi: table 0x%p %s len %lu", table, table_signature, (ulong)table_length);

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

    trace(ACPI_LL, "Acpi: MADT 0x%p", sdt_header);

    if (length < sizeof(ACPISDTHeader) + sizeof(MadtHeader))
    {
        trace(0, "Acpi: MADT too short: %lu", (ulong)length);
        return make_error(stdlib::Error::INVALID_VALUE);
    }

    MadtHeader* header = reinterpret_cast<MadtHeader*>(sdt_header + 1);
    trace(ACPI_LL, "Acpi: MADT LIntCtrl 0x%lX flags 0x%lX",
        (ulong)header->local_int_ctrl_address, (ulong)header->flags);

    const ulong lapic_phys = header->local_int_ctrl_address;
    if (!registers_in_page(lapic_phys, LAPIC_REGISTER_BYTES))
    {
        trace(0, "Acpi: MADT local APIC at 0x%lX, not a page", lapic_phys);
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
        trace(ACPI_LL, "Acpi: MADT entry 0x%p type %lu len %lu",
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

            trace(ACPI_LL, "Acpi: MADT lapic procId %lu apicId %lu flags 0x%lX",
                (ulong)lapic_entry->acpi_process_id, (ulong)lapic_entry->apic_id, (ulong)lapic_entry->flags);

            if (lapic_entry->flags & 0x1)
            {
                if (!CpuTable::get_instance().insert_cpu(lapic_entry->apic_id))
                {
                    trace(ACPI_LL, "Acpi: MADT lapic apicId %lu ignored (max %lu)",
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

            trace(ACPI_LL, "Acpi: MADT ioApicId %lu addr 0x%lX gsi 0x%lX",
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
                trace(0, "Acpi: MADT IO-APIC at 0x%lX, its registers across a page, ignored", io_apic_phys);
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
            break;
        }
        case MADT_ENTRY_TYPE_INT_SRC_OVERRIDE:
        {
            if (entry->length < sizeof(MadtIntSrcOverrideEntry) + sizeof(*entry))
                return make_error(stdlib::Error::INVALID_VALUE);
            MadtIntSrcOverrideEntry* iso_entry = reinterpret_cast<MadtIntSrcOverrideEntry*>(entry + 1);

            trace(ACPI_LL, "Acpi: MADT bus 0x%lX irq 0x%lX gsi 0x%lX flags 0x%lX",
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
        trace(0, "Acpi: MADT names no IO-APIC");
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
        trace(ACPI_LL, "Acpi: no FADT table");
        return;
    }

    ulong body_len = length - sizeof(ACPISDTHeader);
    FadtFields* fadt = reinterpret_cast<FadtFields*>(sdt_header + 1);

    /* Pm1aCntBlk sits at body offset +28; need at least 32 bytes of body */
    static const ulong PM1A_CNT_BLK_END = OFFSET_OF(FadtFields, pm1a_cnt_blk) + sizeof(fadt->pm1a_cnt_blk);
    if (body_len >= PM1A_CNT_BLK_END)
    {
        pm1a_cnt_port_ = fadt->pm1a_cnt_blk;
        trace(ACPI_LL, "Acpi: FADT PM1a_CNT port 0x%lX", pm1a_cnt_port_);
    }

    /* Flags + ResetReg + ResetValue require at least 93 bytes of body (ACPI 2.0+) */
    static const ulong RESET_VALUE_END = OFFSET_OF(FadtFields, reset_value) + sizeof(fadt->reset_value);
    if (body_len >= RESET_VALUE_END)
    {
        trace(ACPI_LL, "Acpi: FADT flags 0x%lX", (ulong)fadt->flags);

        /* RESET_REG_SUP is bit 10 of Flags */
        static const u32 RESET_REG_SUP = (1u << 10);
        if ((fadt->flags & RESET_REG_SUP) && fadt->reset_reg.address_space_id == 1 /* I/O */)
        {
            reset_reg_valid_ = true;
            reset_reg_port_ = (ulong)fadt->reset_reg.address;
            reset_val_ = fadt->reset_value;
            trace(ACPI_LL, "Acpi: FADT RESET_REG port 0x%lX value 0x%lX",
                reset_reg_port_, (ulong)reset_val_);
        }
    }

    /* Century CMOS register selector (ACPI 2.0+, body offset +72). 0 means the
       platform has no century register, so the RTC year must not trust it. */
    static const ulong CENTURY_END = OFFSET_OF(FadtFields, century) + sizeof(fadt->century);
    if (body_len >= CENTURY_END)
    {
        century_register_ = fadt->century;
        trace(ACPI_LL, "Acpi: FADT century register 0x%lX", (ulong)century_register_);
    }
}

u8 Acpi::get_century_register()
{
    return century_register_;
}

void Acpi::parse_hpet()
{
    u32 length = 0;
    ACPISDTHeader* sdt_header = lookup_table("HPET", length);
    if (sdt_header == nullptr)
    {
        trace(ACPI_LL, "Acpi: no HPET table");
        return;
    }

    if (length < sizeof(ACPISDTHeader) + sizeof(HpetTableBody))
    {
        trace(0, "Acpi: HPET table too short: %lu", (ulong)length);
        return;
    }

    HpetTableBody* hpet = reinterpret_cast<HpetTableBody*>(sdt_header + 1);

    /* BaseAddress must be system memory (AddressSpaceId == 0) */
    if (hpet->base_address.address_space_id != 0)
    {
        trace(0, "Acpi: HPET base not in system memory (id %lu)", (ulong)hpet->base_address.address_space_id);
        return;
    }

    hpet_base_phys_ = (ulong)hpet->base_address.address;
    hpet_min_tick_  = hpet->minimum_clock_tick;

    trace(ACPI_LL, "Acpi: HPET base 0x%lX minTick %lu blockId 0x%lX",
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
        trace(ACPI_LL, "Acpi: no WDAT table");
        return;
    }

    if (length < sizeof(ACPISDTHeader) + sizeof(WdatTableBody))
    {
        trace(0, "Acpi: WDAT table too short: %lu", (ulong)length);
        return;
    }

    WdatTableBody* wdat = reinterpret_cast<WdatTableBody*>(sdt_header + 1);

    /* Trust the table length over the Entries count */
    size_t max_entries = (length - sizeof(ACPISDTHeader) - sizeof(WdatTableBody))
        / sizeof(WdatEntry);
    size_t entries = wdat->entries;
    if (entries > max_entries)
    {
        trace(0, "Acpi: WDAT claims %lu entries, table holds %lu",
            (ulong)entries, (ulong)max_entries);
        entries = max_entries;
    }

    const WdatEntry* entry = reinterpret_cast<const WdatEntry*>(wdat + 1);
    for (size_t i = 0; i < entries; i++)
    {
        if (entry[i].register_region.address_space_id == GAS_SPACE_SYSTEM_IO &&
            entry[i].register_region.address == RTC_PORT_BASE)
        {
            trace(0, "Acpi: WDAT drives the RTC, ignoring it");
            return;
        }
    }

    firmware_watchdog_ = true;

    trace(0, "Acpi: WDAT present (%lu entries, period %lu ms), firmware owns the watchdog",
        (ulong)entries, (ulong)wdat->timer_period);
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
        }
    }

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

    return make_error(stdlib::Error::SUCCESS);
}


void* Acpi::get_lapic_address()
{
    return lapic_address_;
}

void* Acpi::get_io_apic_address()
{
    return io_apic_address_;
}

void Acpi::register_irq_to_gsi(u8 irq, u32 gsi, u16 flags)
{
    /* Interrupt::Register takes a GSI in an u8 */
    static const u32 MAX_GSI = 0xFF;

    for (size_t i = 0; i < irq_to_gsi_size_; i++)
    {
        if (irq_to_gsi_[i].irq == irq)
        {
            trace(0, "Acpi: second override of irq %lu (to gsi %lu) ignored", (ulong)irq, (ulong)gsi);
            return;
        }
    }

    if (gsi > MAX_GSI || irq_to_gsi_size_ >= stdlib::array_size(irq_to_gsi_))
    {
        trace(0, "Acpi: override of irq %lu to gsi %lu ignored: %s", (ulong)irq, (ulong)gsi,
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

}