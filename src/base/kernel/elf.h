#pragma once

#include <include/types.h>

namespace kernel
{

/* ELF64, as much of it as the module loader reads (kernel/module.cpp). Field
   names follow the specification's, less the prefixes. */
namespace elf
{

static const ulong IDENT_SIZE = 16;
static const u8 MAGIC[4] = { 0x7F, 'E', 'L', 'F' };
static const ulong IDENT_CLASS = 4;
static const ulong IDENT_DATA = 5;
static const ulong IDENT_VERSION = 6;
static const u8 CLASS64 = 2;
static const u8 DATA2_LSB = 1;
static const u8 VERSION_CURRENT = 1;

static const u16 TYPE_DYN = 3;

static const u16 MACHINE_X86_64 = 62;
static const u16 MACHINE_AARCH64 = 183;

static const u32 PT_LOAD = 1;
static const u32 PT_DYNAMIC = 2;
static const u32 PT_INTERP = 3;
static const u32 PT_TLS = 7;

static const u32 PF_X = 1;
static const u32 PF_W = 2;
static const u32 PF_R = 4;

static const u32 SHT_STRTAB = 3;
static const u32 SHT_REL = 9;
static const u32 SHT_RELA = 4;
static const u32 SHT_DYNSYM = 11;

static const u64 SHF_ALLOC = 2;

static const u16 SHN_UNDEF = 0;
static const u16 SHN_LO_RESERVE = 0xFF00;

static const u8 STB_WEAK = 2;

struct Ehdr
{
    u8 ident[IDENT_SIZE];
    u16 type;
    u16 machine;
    u32 version;
    u64 entry;
    u64 phoff;
    u64 shoff;
    u32 flags;
    u16 ehsize;
    u16 phentsize;
    u16 phnum;
    u16 shentsize;
    u16 shnum;
    u16 shstrndx;
};

struct Phdr
{
    u32 type;
    u32 flags;
    u64 offset;
    u64 vaddr;
    u64 paddr;
    u64 filesz;
    u64 memsz;
    u64 align;
};

struct Shdr
{
    u32 name;
    u32 type;
    u64 flags;
    u64 addr;
    u64 offset;
    u64 size;
    u32 link;
    u32 info;
    u64 addralign;
    u64 entsize;
};

struct Sym
{
    u32 name;
    u8 info;
    u8 other;
    u16 shndx;
    u64 value;
    u64 size;
};

struct Rela
{
    u64 offset;
    u64 info;
    long addend;
};

static_assert(sizeof(Ehdr) == 64, "elf::Ehdr layout");
static_assert(sizeof(Phdr) == 56, "elf::Phdr layout");
static_assert(sizeof(Shdr) == 64, "elf::Shdr layout");
static_assert(sizeof(Sym) == 24, "elf::Sym layout");
static_assert(sizeof(Rela) == 24, "elf::Rela layout");

inline u32 rela_sym(u64 info)
{
    return (u32)(info >> 32);
}

inline u32 rela_type(u64 info)
{
    return (u32)(info & 0xFFFFFFFF);
}

inline u8 sym_bind(u8 info)
{
    return (u8)(info >> 4);
}

}

}
