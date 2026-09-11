#pragma once

#include <include/types.h>

namespace Kernel
{

/* ELF64, as much of it as the module loader reads (kernel/module.cpp). Field
   names follow the specification's, less the prefixes. */
namespace Elf
{

static const ulong IdentSize = 16;
static const u8 Magic[4] = { 0x7F, 'E', 'L', 'F' };
static const ulong IdentClass = 4;
static const ulong IdentData = 5;
static const ulong IdentVersion = 6;
static const u8 Class64 = 2;
static const u8 Data2Lsb = 1;
static const u8 VersionCurrent = 1;

static const u16 TypeDyn = 3;

static const u16 MachineX86_64 = 62;
static const u16 MachineAarch64 = 183;

static const u32 PtLoad = 1;
static const u32 PtDynamic = 2;
static const u32 PtInterp = 3;
static const u32 PtTls = 7;

static const u32 PfX = 1;
static const u32 PfW = 2;
static const u32 PfR = 4;

static const u32 ShtStrtab = 3;
static const u32 ShtRel = 9;
static const u32 ShtRela = 4;
static const u32 ShtDynsym = 11;

static const u64 ShfAlloc = 2;

static const u16 ShnUndef = 0;
static const u16 ShnLoReserve = 0xFF00;

static const u8 StbWeak = 2;

struct Ehdr
{
    u8 Ident[IdentSize];
    u16 Type;
    u16 Machine;
    u32 Version;
    u64 Entry;
    u64 Phoff;
    u64 Shoff;
    u32 Flags;
    u16 Ehsize;
    u16 Phentsize;
    u16 Phnum;
    u16 Shentsize;
    u16 Shnum;
    u16 Shstrndx;
};

struct Phdr
{
    u32 Type;
    u32 Flags;
    u64 Offset;
    u64 Vaddr;
    u64 Paddr;
    u64 Filesz;
    u64 Memsz;
    u64 Align;
};

struct Shdr
{
    u32 Name;
    u32 Type;
    u64 Flags;
    u64 Addr;
    u64 Offset;
    u64 Size;
    u32 Link;
    u32 Info;
    u64 Addralign;
    u64 Entsize;
};

struct Sym
{
    u32 Name;
    u8 Info;
    u8 Other;
    u16 Shndx;
    u64 Value;
    u64 Size;
};

struct Rela
{
    u64 Offset;
    u64 Info;
    long Addend;
};

static_assert(sizeof(Ehdr) == 64, "Elf::Ehdr layout");
static_assert(sizeof(Phdr) == 56, "Elf::Phdr layout");
static_assert(sizeof(Shdr) == 64, "Elf::Shdr layout");
static_assert(sizeof(Sym) == 24, "Elf::Sym layout");
static_assert(sizeof(Rela) == 24, "Elf::Rela layout");

inline u32 RelaSym(u64 info)
{
    return (u32)(info >> 32);
}

inline u32 RelaType(u64 info)
{
    return (u32)(info & 0xFFFFFFFF);
}

inline u8 SymBind(u8 info)
{
    return (u8)(info >> 4);
}

}

}
