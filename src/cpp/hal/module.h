#pragma once

#include <include/types.h>

/* What the loadable-module loader (kernel/module.cpp) has to know about the
   CPU: which ELF machine it is, and what each of its dynamic relocation types
   asks for. A module is a shared object whose static relocations the linker
   has already resolved, so there are few of them. Defined per arch. */
namespace hal
{

u16 module_elf_machine();

enum class ModuleReloc
{
    None,        /* nothing to do */
    Relative,    /* the load base plus the addend */
    Symbol,      /* the symbol's address plus the addend: data, GOT and PLT slots */
    Unsupported,
};

ModuleReloc classify_module_reloc(u32 type);

}
