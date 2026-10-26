#pragma once

#include <include/types.h>

namespace kernel
{

namespace grub
{

/* The Multiboot header. */
struct MultiBootHeader
{
    unsigned long magic;
    unsigned long flags;
    unsigned long checksum;
    unsigned long header_addr;
    unsigned long load_addr;
    unsigned long load_end_addr;
    unsigned long bss_end_addr;
    unsigned long entry_addr;
};

/* The symbol table for a.out. */
struct AoutSymbolTable
{
    unsigned long tab_size;
    unsigned long str_size;
    unsigned long addr;
    unsigned long reserved;
};

/* The section header table for ELF. */
struct ElfSectionHeaderTable
{
    unsigned long num;
    unsigned long size;
    unsigned long addr;
    unsigned long shndx;
};

/* The Multiboot information. */
struct MultiBootInfo
{
    unsigned long flags;
    unsigned long mem_lower;
    unsigned long mem_upper;
    unsigned long boot_device;
    unsigned long cmd_line;
    unsigned long mods_count;
    unsigned long mods_addr;
    union
    {
        AoutSymbolTable aout_sym;
        ElfSectionHeaderTable elf_sec;
    } u;
    unsigned long mmap_length;
    unsigned long mmap_addr;
};

/* The module structure. */
struct Module
{
    unsigned long mod_start;
    unsigned long mod_end;
    unsigned long string;
    unsigned long reserved;
};

/* The memory map. Be careful that the offset 0 is base_addr_low
but no size. */
struct MemoryMap
{
    unsigned long size;
    unsigned long base_addr_low;
    unsigned long base_addr_high;
    unsigned long length_low;
    unsigned long length_high;
    unsigned long type;
};


struct MultiBootTag
{
    u32 type;
    u32 size;
};

struct MultiBootInfoHeader
{
    u32 total_size;
    u32 reserved;
};

struct MultiBootTagString
{
    u32 type;
    u32 size;
    char string[0];
};

struct MultiBootTagModule
{
    u32 type;
    u32 size;
    u32 mod_start;
    u32 mod_end;
    char cmd_line[0];
};

struct MultiBootTagBootDev
{
    u32 type;
    u32 size;
    u32 bios_dev;
    u32 slice;
    u32 part;
};

struct MultiBootMmapEntry
{
    u64 addr;
    u64 len;
    u32 type;
    u32 zero;
} __attribute__((packed));

const u32 MULTI_BOOT_MEMORY_AVAILABLE = 1;
const u32 MULTI_BOOT_MEMORY_RESERVED = 2;
const u32 MULTI_BOOT_MEMORY_ACPI_RECLAIMABLE = 3;
const u32 MULTI_BOOT_MEMORY_NVS = 4;

struct MultiBootTagMmap
{
    u32 type;
    u32 size;
    u32 entry_size;
    u32 entry_version;
    MultiBootMmapEntry entry[0];  
};

struct MultiBootTagFramebuffer
{
    u32 type;
    u32 size;
    u64 addr;
    u32 pitch;
    u32 width;
    u32 height;
    u8 bpp;
    u8 fb_type;
    u16 reserved;
    /* Present only when fb_type == Rgb (the indexed variant carries a
       palette here instead), so a tag may legitimately be shorter. */
    u8 red_field_position;
    u8 red_mask_size;
    u8 green_field_position;
    u8 green_mask_size;
    u8 blue_field_position;
    u8 blue_mask_size;
} __attribute__((packed));

/* Bytes of MultiBootTagFramebuffer every framebuffer tag has, i.e. up to
   and including Reserved. */
const u32 MULTI_BOOT_TAG_FRAMEBUFFER_COMMON_SIZE = 32;

const u8 MULTI_BOOT_FRAMEBUFFER_TYPE_INDEXED = 0;
const u8 MULTI_BOOT_FRAMEBUFFER_TYPE_RGB = 1;
const u8 MULTI_BOOT_FRAMEBUFFER_TYPE_EGA_TEXT = 2;

/* Tags 14/15 carry a verbatim copy of the ACPI RSDP (v1/v2). */
struct MultiBootTagAcpi
{
    u32 type;
    u32 size;
    u8 rsdp[0];
};

const u32 MULTI_BOOT_TAG_TYPE_END = 0;
const u32 MULTI_BOOT_TAG_TYPE_MODULE = 3;
const u32 MULTI_BOOT_TAG_TYPE_MMAP = 6;
const u32 MULTI_BOOT_TAG_TYPE_BOOT_DEV = 5;
const u32 MULTI_BOOT_TAG_TYPE_CMDLINE = 1;
const u32 MULTI_BOOT_TAG_TYPE_FRAMEBUFFER = 8;
const u32 MULTI_BOOT_TAG_TYPE_ACPI_OLD = 14;
const u32 MULTI_BOOT_TAG_TYPE_ACPI_NEW = 15;

void parse_multi_boot_info(MultiBootInfoHeader *mb_info);

/* RSDP copy saved from the ACPI tag, if GRUB provided one (it always
   does on UEFI, where the legacy BIOS-area scan cannot find the RSDP).
   Returns nullptr and size 0 if no ACPI tag was present. */
const void* get_acpi_rsdp(size_t& size);

/* The framebuffer GRUB set up for us, saved from the framebuffer tag.
   Color fields are only filled in for Type == Rgb. */
struct FramebufferInfo
{
    u64 addr;
    u32 pitch;
    u32 width;
    u32 height;
    u8 bpp;
    u8 type;
    u8 red_pos;
    u8 red_size;
    u8 green_pos;
    u8 green_size;
    u8 blue_pos;
    u8 blue_size;
};

/* The physical range of the module GRUB loaded with the command line
   `ucode` -- this CPU's microcode, `module2 /boot/ucode ucode` -- for the
   BSP to choose an update from while the bootstrap map still reaches it
   (microcode::load_on_bsp). False when there is none. */
bool get_ucode_module(ulong& phys, ulong& len);

/* True if GRUB provided a framebuffer tag. */
bool has_framebuffer_info();

/* True if the framebuffer is legacy EGA text mode at 0xB8000.
   Only meaningful when has_framebuffer_info() is true. */
bool is_framebuffer_ega_text();

/* The saved framebuffer tag, or nullptr if GRUB provided none. */
const FramebufferInfo* get_framebuffer_info();

}
}