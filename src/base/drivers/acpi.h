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

    /* The RSDP's OEM id, six characters: whose firmware this is -- "BOCHS "
       is QEMU's */
    const char* get_oem_id();

    void* get_lapic_address();

    void* get_io_apic_address();
    /* The MADT's id of the IO-APIC get_io_apic_address maps -- what a DMAR's
       IO-APIC scope names it by (8.3.1.1) -- valid while that is not null */
    u8 get_io_apic_id();

    u32 get_gsi_by_irq(u8 irq);
    u16 get_irq_flags(u8 irq);

    /* FADT: PM1a control port for ACPI S5 shutdown (0 if not found) */
    ulong get_pm1a_cnt_port();

    /* FADT: PM1b control port, 0 where the machine has no second block --
       as most have none */
    ulong get_pm1b_cnt_port();

    /* The DSDT's \_S5: the sleep types that, written to the PM1 control
       registers with SLP_EN, turn the machine off. They are the chipset's
       own -- 7 on Intel's PCHs, 5 on AMD's FCH, 0 under QEMU -- so one that
       is guessed asks for another state: the 5 this kernel used to write
       everywhere is S3 on an Intel machine, a suspend to RAM with nothing
       set up to come back from it. False when the firmware's DSDT gave no
       \_S5 this kernel can read, which Parse has said. */
    bool get_s5(u8& typ_a, u8& typ_b);

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

    /* DMAR (Intel VT-d 4.1, chapter 8): the DMA remapping units and what the
       firmware says of them, as far as this kernel uses them -- each unit
       (DRHD) a register block and the devices it covers, and the memory
       regions (RMRR) some devices' DMA must keep reaching. Copied out and
       bounded at boot as every table's fields are. A DMAR this does not
       hold all of -- a structure cut by the table or by its own length, more
       units, scopes, regions or hops of a path than are kept -- is said, and
       dmar_complete() is false: a unit left out would leave its devices
       with no translation at all once the others are turned on. */
    static const size_t MAX_DMAR_UNITS = 8;
    static const size_t MAX_DMAR_SCOPES = 16;
    static const size_t MAX_DMAR_RESERVED = 8;
    static const size_t MAX_DMAR_PATH = 4;

    /* A device scope (8.3.1): a PCI device by its path from start_bus -- a
       (device, function) pair a hop, the bridges' secondary buses between --
       or an IO-APIC or HPET by its id, whose path is its source-id's */
    struct DmarScope
    {
        u8 type;
        u8 enumeration_id;
        u8 start_bus;
        u8 path_length;
        u8 path[MAX_DMAR_PATH][2];
    };

    static const u8 DMAR_SCOPE_ENDPOINT = 1;
    static const u8 DMAR_SCOPE_BRIDGE = 2;
    static const u8 DMAR_SCOPE_IOAPIC = 3;
    static const u8 DMAR_SCOPE_HPET = 4;
    static const u8 DMAR_SCOPE_NAMESPACE = 5;

    /* A remapping unit (8.3): its registers, and the devices it translates --
       those its scopes name, or with include_all every device of its
       segment no other unit names */
    struct DmarUnit
    {
        ulong base;
        u16 segment;
        bool include_all;
        /* Its registers take 2^size pages (VT-d 3.0's field; 0 -- one page --
           in a table older than that) */
        u8 size;
        size_t scope_count;
        DmarScope scope[MAX_DMAR_SCOPES];
    };

    /* A reserved memory region (8.4): limit is its last byte */
    struct DmarReserved
    {
        ulong base;
        ulong limit;
        u16 segment;
        size_t scope_count;
        DmarScope scope[MAX_DMAR_SCOPES];
    };

    /* The DMAR's flags (8.1) */
    static const u8 DMAR_INTR_REMAP = 1 << 0;
    static const u8 DMAR_X2APIC_OPT_OUT = 1 << 1;
    static const u8 DMAR_PLATFORM_OPT_IN = 1 << 2;

    bool has_dmar();
    bool dmar_complete();
    /* The widest DMA address the platform makes, in bits (HAW + 1) */
    u8 get_dmar_width();
    u8 get_dmar_flags();
    size_t get_dmar_unit_count();
    /* nullptr past the count */
    const DmarUnit* get_dmar_unit(size_t index);
    size_t get_dmar_reserved_count();
    const DmarReserved* get_dmar_reserved(size_t index);

    /* MCFG (PCI Firmware 3.2, 4.1.2): segment 0's ECAM window -- its
       physical base, 0 where the table names none, and the buses it
       decodes, start to end */
    ulong get_mcfg_base();
    u8 get_mcfg_start_bus();
    u8 get_mcfg_end_bus();

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
    static const size_t MAX_TABLES = 8;
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

    /* The DSDT is not one of the tables above: a real one is hundreds of
       KiB -- the window has no room to hold it whole, nor is it to be held
       to MAX_TABLE_LENGTH -- and all this kernel wants of it is one name.
       It is read after the tables are let go of, a page of it mapped at a
       time, and one longer than this is not read at all. */
    static const u32 MAX_DSDT_LENGTH = 4 * 1024 * 1024;

    /* AML (ACPI 20.2), as much of it as a \_S5 is written in: the name
       defined by NameOp, at the root with or without the root's character,
       as a package whose first two elements are the sleep types, each a
       constant of a byte. Nothing here runs AML: a firmware that computes
       its \_S5, or keeps it in an SSDT, has none this kernel reads. */
    static const u8 AML_ZERO_OP = 0x00;
    static const u8 AML_ONE_OP = 0x01;
    static const u8 AML_NAME_OP = 0x08;
    static const u8 AML_BYTE_PREFIX = 0x0A;
    static const u8 AML_PACKAGE_OP = 0x12;
    static const u8 AML_ROOT_CHAR = 0x5C;

    /* A PkgLength's lead byte: its top two bits count the bytes after it.
       With none the length is its low six bits; else its low four, and the
       bytes after it above those. */
    static const u8 AML_PKG_FOLLOW_SHIFT = 6;
    static const u8 AML_PKG_SHORT_MASK = 0x3F;
    static const u8 AML_PKG_LOW_MASK = 0x0F;
    static const u8 AML_PKG_LOW_BITS = 4;

    /* SLP_TYP is three bits of a PM1 control register */
    static const u8 SLP_TYP_MAX = 7;

    /* The most of the AML a \_S5 is read from: NameOp, the root's
       character, the name, PackageOp, a PkgLength of four bytes, the count
       of elements, and two elements of a prefix and a byte each */
    static const size_t S5_MAX_BYTES = 1 + 1 + 4 + 1 + 4 + 1 + 2 + 2;

    /* Whether the len bytes at aml begin with a \_S5 as above, and its
       two sleep types if so. Nothing past len is read, and nothing of the
       package past where its own length ends it. */
    static bool parse_s5(const u8* aml, size_t len, u8& typ_a, u8& typ_b);

    /* An element of that package at aml[at], which it moves past: a
       constant no greater than SLP_TYP_MAX, read from before limit */
    static bool parse_s5_type(const u8* aml, size_t limit, size_t& at, u8& value);

    /* The DSDT the FADT named, read for its \_S5 */
    void parse_dsdt();

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

    /* The GSI base of the IO-APIC io_apic_address_ maps, ~0U while none,
       and its id */
    u32 io_apic_gsi_base_;
    u8 io_apic_id_;

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
        u16 arm_boot_arch;       /* +93 */
        u8  fadt_minor_version;  /* +95 */
        u64 x_firmware_ctrl;     /* +96 */
        u64 x_dsdt;              /* +104 */
    } __attribute__((packed));

    static_assert(OFFSET_OF(FadtFields, pm1b_cnt_blk) == 32, "Invalid FADT layout");
    static_assert(OFFSET_OF(FadtFields, reset_value) == 92, "Invalid FADT layout");
    static_assert(OFFSET_OF(FadtFields, x_dsdt) == 104, "Invalid FADT layout");

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

    /* The DMAR's fixed part, after the header: the host address width less
       one, the flags, ten reserved bytes; then its remapping structures, a
       type and a length each (8.1, 8.2) */
    static const size_t DMAR_BODY_BYTES = 12;
    static const size_t DMAR_STRUCT_HEADER_BYTES = 4;
    static const u16 DMAR_TYPE_DRHD = 0;
    static const u16 DMAR_TYPE_RMRR = 1;
    /* What of a DRHD and an RMRR comes before its device scopes */
    static const size_t DRHD_FIXED_BYTES = 16;
    static const size_t RMRR_FIXED_BYTES = 24;
    static const u8 DRHD_INCLUDE_PCI_ALL = 1 << 0;
    /* A device scope: type, length, flags, reserved, enumeration id, start
       bus, then a path of two bytes a hop */
    static const size_t SCOPE_FIXED_BYTES = 6;

    void parse_dmar();

    /* The device scopes in the len bytes at scopes, into scope; false, the
       table said to be more than is kept, where one is cut by them or by its
       own length, or there are more than MAX_DMAR_SCOPES or a path longer
       than MAX_DMAR_PATH */
    static bool parse_dmar_scopes(const u8* scopes, size_t len, DmarScope* scope, size_t& count);

    /* MCFG's body: eight reserved bytes, then an allocation a 16 bytes --
       the base, the segment, the start and end bus, four reserved */
    static const size_t MCFG_RESERVED_BYTES = 8;
    static const size_t MCFG_ENTRY_BYTES = 16;

    void parse_mcfg();

    /* The SRAT (ACPI 6.5, 5.2.16): after the header four bytes the spec
       has as 1 and eight reserved, then structures of a type and a length
       each -- a CPU by its local APIC id (16 bytes) or its x2APIC id (24),
       or a range of memory (40), in a proximity domain, enabled by bit 0 of
       their flags. A CPU's entry gives its domain's low byte, and from the
       table's revision 2 on the other three after its SAPIC EID. What it
       says goes to Numa, which the kernel seals once the tables are read. */
    static const size_t SRAT_BODY_BYTES = 12;
    static const size_t SRAT_STRUCT_HEADER_BYTES = 2;
    static const u8 SRAT_TYPE_LAPIC = 0;
    static const u8 SRAT_TYPE_MEMORY = 1;
    static const u8 SRAT_TYPE_X2APIC = 2;
    static const size_t SRAT_LAPIC_BYTES = 16;
    static const size_t SRAT_MEMORY_BYTES = 40;
    static const size_t SRAT_X2APIC_BYTES = 24;
    static const u32 SRAT_ENABLED = 1 << 0;
    static const u8 SRAT_WIDE_DOMAIN_REVISION = 2;

    void parse_srat();

    /* The SLIT (5.2.17): the number of localities, eight bytes, and then a
       byte a pair of them, row by row -- the distance from the row's to the
       column's, a locality being a proximity domain by its number. Read after
       the SRAT, for the domains it named. */
    static const size_t SLIT_BODY_BYTES = 8;

    void parse_slit();

    /* FADT-derived values */
    ulong pm1a_cnt_port_;
    ulong pm1b_cnt_port_;
    /* Where the FADT says the DSDT is, 0 when it names none */
    ulong dsdt_phys_;

    /* DSDT-derived values: its \_S5, when s5_valid_ */
    bool s5_valid_;
    u8 s5_typ_a_;
    u8 s5_typ_b_;
    bool reset_reg_valid_;
    ulong reset_reg_port_;
    u8 reset_val_;
    u8 century_register_;

    /* HPET-derived values */
    ulong hpet_base_phys_;
    u16 hpet_min_tick_;

    /* WDAT-derived value */
    bool firmware_watchdog_;

    /* DMAR-derived values */
    bool dmar_present_;
    bool dmar_complete_;
    u8 dmar_width_;
    u8 dmar_flags_;
    size_t dmar_unit_count_;
    DmarUnit dmar_unit_[MAX_DMAR_UNITS];
    size_t dmar_reserved_count_;
    DmarReserved dmar_reserved_[MAX_DMAR_RESERVED];

    /* MCFG-derived values */
    ulong mcfg_base_;
    u8 mcfg_start_bus_;
    u8 mcfg_end_bus_;

};

}