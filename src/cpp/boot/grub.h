#pragma once

#include <include/types.h>

namespace Kernel
{

namespace Grub
{

/* The Multiboot header. */
struct MultiBootHeader
{
    unsigned long Magic;
    unsigned long Flags;
    unsigned long Checksum;
    unsigned long HeaderAddr;
    unsigned long LoadAddr;
    unsigned long LoadEndAddr;
    unsigned long BssEndAddr;
    unsigned long EntryAddr;
};

/* The symbol table for a.out. */
struct AoutSymbolTable
{
    unsigned long TabSize;
    unsigned long StrSize;
    unsigned long Addr;
    unsigned long Reserved;
};

/* The section header table for ELF. */
struct ElfSectionHeaderTable
{
    unsigned long Num;
    unsigned long Size;
    unsigned long Addr;
    unsigned long Shndx;
};

/* The Multiboot information. */
struct MultiBootInfo
{
    unsigned long Flags;
    unsigned long MemLower;
    unsigned long MemUpper;
    unsigned long BootDevice;
    unsigned long CmdLine;
    unsigned long ModsCount;
    unsigned long ModsAddr;
    union
    {
        AoutSymbolTable AoutSym;
        ElfSectionHeaderTable ElfSec;
    } u;
    unsigned long MmapLength;
    unsigned long MmapAddr;
};

/* The module structure. */
struct Module
{
    unsigned long ModStart;
    unsigned long ModEnd;
    unsigned long String;
    unsigned long Reserved;
};

/* The memory map. Be careful that the offset 0 is base_addr_low
but no size. */
struct MemoryMap
{
    unsigned long Size;
    unsigned long BaseAddrLow;
    unsigned long BaseAddrHigh;
    unsigned long LengthLow;
    unsigned long LengthHigh;
    unsigned long Type;
};


struct MultiBootTag
{
    u32 Type;
    u32 Size;
};

struct MultiBootInfoHeader
{
    u32 TotalSize;
    u32 Reserved;
};

struct MultiBootTagString
{
    u32 Type;
    u32 Size;
    char String[0];
};

struct MultiBootTagModule
{
    u32 Type;
    u32 Size;
    u32 ModStart;
    u32 ModEnd;
    char CmdLine[0];
};

struct MultiBootTagBootDev
{
    u32 Type;
    u32 Size;
    u32 BiosDev;
    u32 Slice;
    u32 Part;
};

struct MultiBootMmapEntry
{
    u64 Addr;
    u64 Len;
    u32 Type;
    u32 Zero;
} __attribute__((packed));

const u32 MultiBootMemoryAvailable = 1;
const u32 MultiBootMemoryReserved = 2;
const u32 MultiBootMemoryAcpiReclaimable = 3;
const u32 MultiBootMemoryNvs = 4;

struct MultiBootTagMmap
{
    u32 Type;
    u32 Size;
    u32 EntrySize;
    u32 EntryVersion;
    MultiBootMmapEntry Entry[0];  
};

struct MultiBootTagFramebuffer
{
    u32 Type;
    u32 Size;
    u64 Addr;
    u32 Pitch;
    u32 Width;
    u32 Height;
    u8 Bpp;
    u8 FbType;
    u8 Reserved;
} __attribute__((packed));

const u8 MultiBootFramebufferTypeIndexed = 0;
const u8 MultiBootFramebufferTypeRgb = 1;
const u8 MultiBootFramebufferTypeEgaText = 2;

/* Tags 14/15 carry a verbatim copy of the ACPI RSDP (v1/v2). */
struct MultiBootTagAcpi
{
    u32 Type;
    u32 Size;
    u8 Rsdp[0];
};

const u32 MultiBootTagTypeEnd = 0;
const u32 MultiBootTagTypeMmap = 6;
const u32 MultiBootTagTypeBootDev = 5;
const u32 MultiBootTagTypeCmdline = 1;
const u32 MultiBootTagTypeFramebuffer = 8;
const u32 MultiBootTagTypeAcpiOld = 14;
const u32 MultiBootTagTypeAcpiNew = 15;

void ParseMultiBootInfo(MultiBootInfoHeader *MbInfo);

/* RSDP copy saved from the ACPI tag, if GRUB provided one (it always
   does on UEFI, where the legacy BIOS-area scan cannot find the RSDP).
   Returns nullptr and size 0 if no ACPI tag was present. */
const void* GetAcpiRsdp(size_t& size);

/* True if GRUB provided a framebuffer tag. */
bool HasFramebufferInfo();

/* True if the framebuffer is legacy EGA text mode at 0xB8000.
   Only meaningful when HasFramebufferInfo() is true. */
bool IsFramebufferEgaText();

}
}