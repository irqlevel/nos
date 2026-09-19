#include "module_exports.h"
#include "symtab.h"

/* The tables the Makefile makes from pass1.elf -- the module export table
   (module_exports.S) and the symbol table (symtab_data.cpp) -- are linked into
   the final kernel only, and these empty weak ones stand in for them in
   pass 1. They live in a file of their own, one that indexes neither: defined
   beside the code that looked the tables up, an empty one gave that code an
   array of type T[0], and every index into it was out of bounds as far as the
   compiler was concerned -- what a UBSan build reported (docs/build.md). The
   code that indexes them sees the declarations alone. */
__attribute__((weak)) const ModuleExport nos_module_exports[] = {};
__attribute__((weak)) const ulong nos_module_export_count = 0;

namespace Kernel
{

__attribute__((weak)) const SymEntry SymbolTable::Symbols[] = {};
__attribute__((weak)) const size_t SymbolTable::SymbolCount = 0;

}
