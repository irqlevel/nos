#pragma once

#include <lib/stdlib.h>
#include <lib/error.h>

namespace kernel
{

class Acpi final
{
public:
    static Acpi& get_instance()
    {
        static Acpi instance;
        return instance;
    }

    stdlib::Error parse();

    void* get_lapic_address();

    void* get_io_apic_address();

    u32 get_gsi_by_irq(u8 irq);
    u16 get_irq_flags(u8 irq);

    /* FADT: PM1a control port for ACPI S5 shutdown (0 if not found) */
    ulong get_pm1a_cnt_port();

    /* FADT: ACPI reset register (I/O port) and value (0/false if not available) */
    bool has_reset_reg();
    ulong get_reset_reg_port();
    u8 get_reset_value();

    /* FADT: RTC century CMOS register selector (0 if the platform has none) */
    u8 get_century_register();

    /* HPET: physical base address (0 if no HPET ACPI table) */
    ulong get_hpet_base_phys();
    u16 get_hpet_min_tick();

    /* WDAT: true when the firmware describes a watchdog for the OS to drive
       through the ACPI instruction set.  Native watchdog drivers (the Rust
       tco_wdt) must then leave the hardware alone -- the same rule Linux
       applies in acpi_has_watchdog(). */
    bool has_firmware_watchdog();

private:
    Acpi();
    ~Acpi();
    Acpi(const Acpi& other) = delete;
    Acpi(Acpi&& other) = delete;
    Acpi& operator=(const Acpi& other) = delete;
    Acpi& operator=(Acpi&& other) = delete;

    struct RSDPDescriptor
    {
        u64 signature;
        u8 checksum;
        char oemid[6];
        u8 revision;
        u32 rsdt_address;
    } __attribute__((packed));

    struct RSDPDescriptor20
    {
        RSDPDescriptor first_part;
        u32 length;
        u64 xsdt_address;
        u8 extended_checksum;
        u8 reserved[3];
    } __attribute__((packed));

    struct ACPISDTHeader
    {
        char signature[4];
        u32 length;
        u8 revision;
        u8 checksum;
        char oemid[6];
        char oem_table_id[8];
        u32 oem_revision;
        u32 creator_id;
        u32 creator_revision;
        u32 entry[0];
    } __attribute__((packed));

    static_assert(sizeof(ACPISDTHeader) == 36, "Invalid size");

    struct MadtEntry
    {
        u8 type;
        u8 length;
    } __attribute__((packed));

    static const u8 MADT_ENTRY_TYPE_LAPIC = 0;
    static const u8 MADT_ENTRY_TYPE_IO_APIC = 1;
    static const u8 MADT_ENTRY_TYPE_INT_SRC_OVERRIDE = 2;

    struct MadtHeader
    {
        u32 local_int_ctrl_address;
        u32 flags;
        MadtEntry entry[0];
    } __attribute__((packed));

    struct MadtLapicEntry
    {
        u8 acpi_process_id;
        u8 apic_id;
        u32 flags;
    } __attribute__((packed));

    struct MadtIoApicEntry
    {
        u8 io_apic_id;
        u8 reserved;
        u32 io_apic_address;
        u32 global_system_interrupt_base;
    } __attribute__((packed));

    struct MadtIntSrcOverrideEntry
    {
        u8 bus_source;
        u8 irq_source;
        u32 global_system_interrupt;
        u16 flags;
    } __attribute__((packed));

    int compute_sum(void* table, size_t len);

    /* All three yield the physical address of the root table and which of
       the two kinds it is. See parse_rsdp for why there are two. */
    bool parse_rsdp(RSDPDescriptor20* rsdp, ulong& root_phys_addr, bool& is_xsdt);
    bool find_root_table(ulong& root_phys_addr, bool& is_xsdt);
    bool scan_rsdp_range(ulong phy_start, ulong phy_end, ulong& root_phys_addr,
        bool& is_xsdt);
    stdlib::Error parse_root_table(ACPISDTHeader* root);

    /* Root entry index -> physical address of that table. The entries are
       32-bit in an RSDT and 64-bit in an XSDT, and in an XSDT they are not
       naturally aligned (the header is 36 bytes), so they are copied out
       rather than dereferenced. */
    ulong root_entry(size_t index);

    /* Parse's work: what it maps is left for Parse to let go of */
    stdlib::Error parse_tables();
    stdlib::Error parse_table_pointers();
    stdlib::Error parse_madt();

    /* The table of that signature Parse is reading, and its length */
    ACPISDTHeader* lookup_table(const char *name, u32& length);

    char oem_id_[7];

    ACPISDTHeader* root_;
    u32 root_length_;
    bool root_is_xsdt_;

    /* The tables this kernel reads, one of each kind, mapped through the
       tmp_map window while Parse reads them: Table[i] is WANTED_TABLES[i]'s,
       table_length_[i] its length -- what of it is mapped, and all a parser
       reads. Every other table the root lists is looked at and let go of at
       once, and these and the root when Parse is done, whichever way it
       ends: what the kernel keeps of ACPI is the values Parse takes out of
       it, and the local APIC's and the IO-APIC's pages. A firmware's table
       count is not small and not predictable -- the Hetzner EX44 lists 25,
       eleven of them SSDTs, and the count moves with every BIOS revision --
       and a slot of the window held is one every page allocation after it
       goes without. */
    static const size_t MAX_TABLES = 4;
    ACPISDTHeader* table_[MAX_TABLES];
    u32 table_length_[MAX_TABLES];

    /* The longest table mapped, the root or one of those read -- a MADT of
       a thousand CPUs, each with its x2APIC and NMI entries, is 42 KiB. A
       longer one is refused, so that what Parse holds of the window at once
       is bounded: the root and a table of each kind, at most 17 pages
       each. */
    static const u32 MAX_TABLE_LENGTH = 64 * 1024;

    /* WANTED_TABLES' index of signature, or -1 */
    static long wanted_index(const char* signature);

    /* The header of the SDT at phys, mapped -- SDTs are only 4-byte
       aligned, so it may straddle two pages -- or nullptr, which is said:
       past the physical address space, or no room in the window */
    ACPISDTHeader* map_header(ulong phys);

    /* The SDT at phys, whose header map_header mapped, mapped whole, its
       length read once into length; the header's mapping is let go of.
       nullptr, which is said, for a table not to be mapped -- shorter than
       its header, longer than MAX_TABLE_LENGTH, reaching past the physical
       address space -- or no room in the window */
    ACPISDTHeader* map_whole(ACPISDTHeader* header, ulong phys, u32& length);

    /* Let go of the root and the tables */
    void release_tables();

    /* Let go of the tmp_map pages a mapping of len bytes at va holds */
    static void unmap_table_range(void* va, ulong len);

    static const bool CHECK_RSDT_CHECKSUM = false;
    static const u64 RSDP_SIGNATURE = 0x2052545020445352ULL; //'RSD PTR '

    void* lapic_address_;
    void* io_apic_address_;

    struct IrqToGsiEntry
    {
        u8 irq;
        u32 gsi;
        u16 flags;
    };

    IrqToGsiEntry irq_to_gsi_[64];
    size_t irq_to_gsi_size_;

    /* An Interrupt Source Override: the first for an IRQ is the one taken,
       and one naming a GSI past what the interrupt layer takes (an u8) or
       past the table's room is left out, which is said -- none of them is
       reason enough to lose ACPI, and the boot with it. */
    void register_irq_to_gsi(u8 irq, u32 gsi, u16 flags);

    /* The GSI base of the IO-APIC io_apic_address_ maps, ~0U while none */
    u32 io_apic_gsi_base_;

    /* What of an APIC's page its registers take, from the address the MADT
       gives. The local APIC's are the page, which the architecture puts at
       a page's start (IA32_APIC_BASE holds a page frame); an IO-APIC's are
       words at 0x00 and 0x10, and at 0x40 the EOI register of a later one.
       An address whose registers would run past its page is refused: the
       drivers' accesses would land in the tmp_map window's next slot, and
       in whatever page that maps. */
    static const ulong LAPIC_REGISTER_BYTES = consts::PAGE_SIZE;
    static const ulong IO_APIC_REGISTER_BYTES = 0x44;

    /* Whether registers of that many bytes at phys stay in its page */
    static bool registers_in_page(ulong phys, ulong bytes);

    /* Generic Address Structure (ACPI spec 5.2.3.2) */
    struct GenericAddressStructure
    {
        u8  address_space_id;  /* 0 = system memory, 1 = I/O, 2 = PCI config */
        u8  register_bit_width;
        u8  register_bit_offset;
        u8  access_size;
        u64 address;
    } __attribute__((packed));

    static_assert(sizeof(GenericAddressStructure) == 12, "Invalid GAS size");

    /* FADT fields we care about (ACPI spec offsets beyond SDT header) */
    struct FadtFields
    {
        /* offset  0 (from after SDT header = absolute offset 36): firmware_ctrl */
        u32 firmware_ctrl;       /* +0  */
        u32 dsdt;               /* +4  */
        u8  reserved0;          /* +8  */
        u8  preferred_pm_profile; /* +9  */
        u16 sci_int;             /* +10 */
        u32 smi_cmd;             /* +12 */
        u8  acpi_enable;         /* +16 */
        u8  acpi_disable;        /* +17 */
        u8  s4_bios_req;          /* +18 */
        u8  pstate_ctrl;         /* +19 */
        u32 pm1a_evt_blk;         /* +20 */
        u32 pm1b_evt_blk;         /* +24 */
        u32 pm1a_cnt_blk;         /* +28 */
        u32 pm1b_cnt_blk;         /* +32 */
        u32 pm2_cnt_blk;          /* +36 */
        u32 pm_tmr_blk;           /* +40 */
        u32 gpe0_blk;            /* +44 */
        u32 gpe1_blk;            /* +48 */
        u8  pm1_evt_len;          /* +52 */
        u8  pm1_cnt_len;          /* +53 */
        u8  pm2_cnt_len;          /* +54 */
        u8  pm_tmr_len;           /* +55 */
        u8  gpe0_blk_len;         /* +56 */
        u8  gpe1_blk_len;         /* +57 */
        u8  gpe1_base;           /* +58 */
        u8  cst_cnt;             /* +59 */
        u16 p_lvl2_lat;           /* +60 */
        u16 p_lvl3_lat;           /* +62 */
        u16 flush_size;          /* +64 */
        u16 flush_stride;        /* +66 */
        u8  duty_offset;         /* +68 */
        u8  duty_width;          /* +69 */
        u8  day_alarm;           /* +70 */
        u8  mon_alarm;           /* +71 */
        u8  century;            /* +72 */
        u16 ia_pc_boot_arch;       /* +73 */
        u8  reserved1;          /* +75 */
        u32 flags;              /* +76 */
        GenericAddressStructure reset_reg;   /* +80 */
        u8  reset_value;         /* +92 */
    } __attribute__((packed));

    /* HPET ACPI table body (beyond SDT header) */
    struct HpetTableBody
    {
        u32 event_timer_block_id;          /* +0  */
        GenericAddressStructure base_address; /* +4  */
        u8  hpet_number;                 /* +16 */
        u16 minimum_clock_tick;           /* +17 */
        u8  page_protection;             /* +19 */
    } __attribute__((packed));

    /* WDAT ACPI table body (beyond the SDT header), followed by Entries
       WdatEntry records */
    struct WdatTableBody
    {
        u32 header_length;       /* +0  */
        u16 pci_segment;         /* +4  */
        u8  pci_bus;             /* +6  */
        u8  pci_device;          /* +7  */
        u8  pci_function;        /* +8  */
        u8  reserved[3];        /* +9  */
        u32 timer_period;        /* +12, milliseconds per count */
        u32 max_count;           /* +16 */
        u32 min_count;           /* +20 */
        u8  flags;              /* +24 */
        u8  reserved2[3];       /* +25 */
        u32 entries;            /* +28 */
    } __attribute__((packed));

    static_assert(sizeof(WdatTableBody) == 32, "Invalid WDAT body size");

    struct WdatEntry
    {
        u8  action;             /* +0  */
        u8  instruction;        /* +1  */
        u16 reserved;           /* +2  */
        GenericAddressStructure register_region; /* +4  */
        u32 value;              /* +16 */
        u32 mask;               /* +20 */
    } __attribute__((packed));

    static_assert(sizeof(WdatEntry) == 24, "Invalid WDAT entry size");

    /* A WDAT whose instructions poke the RTC is ignored: the CMOS ports are
       not the watchdog's alone (Linux does the same in
       acpi_watchdog_uses_rtc). */
    static const u8 GAS_SPACE_SYSTEM_IO = 1;
    static const u64 RTC_PORT_BASE = 0x70;

    void parse_fadt();
    void parse_hpet();
    void parse_wdat();

    /* FADT-derived values */
    ulong pm1a_cnt_port_;
    bool reset_reg_valid_;
    ulong reset_reg_port_;
    u8 reset_val_;
    u8 century_register_;

    /* HPET-derived values */
    ulong hpet_base_phys_;
    u16 hpet_min_tick_;

    /* WDAT-derived value */
    bool firmware_watchdog_;

};

}