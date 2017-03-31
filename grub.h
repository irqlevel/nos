#pragma once

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

}
}