#include "symtab.h"
#include "module.h"

/* The kernel's text, from the linker script: only an address in it is one of
   Symbols[]' -- past the last function, a loadable module's code say, is not
   that function, however near the offset would come out */
extern "C" char KernelStart[], KernelText[];

namespace Kernel
{

/* Weak defaults — overridden by generated symtab_data.o in pass 2 */
__attribute__((weak)) const SymEntry SymbolTable::Symbols[] = {};
__attribute__((weak)) const size_t SymbolTable::SymbolCount = 0;

SymbolTable::SymbolTable()
{
}

SymbolTable::~SymbolTable()
{
}

bool SymbolTable::Resolve(ulong addr, const char*& name, ulong& offset)
{
    if (SymbolCount == 0)
        return false;

    if (addr < reinterpret_cast<ulong>(KernelStart) || addr >= reinterpret_cast<ulong>(KernelText))
        return false;

    /* Binary search for the largest Symbols[i].Addr <= addr */
    size_t lo = 0;
    size_t hi = SymbolCount;

    while (lo < hi)
    {
        size_t mid = lo + (hi - lo) / 2;
        if (Symbols[mid].Addr <= addr)
            lo = mid + 1;
        else
            hi = mid;
    }

    if (lo == 0)
        return false;

    lo--;
    name = Symbols[lo].Name;
    offset = addr - Symbols[lo].Addr;
    return true;
}

bool SymbolTable::Describe(ulong addr, char* buf, ulong size)
{
    const char* name;
    ulong offset;
    if (Resolve(addr, name, offset))
    {
        Stdlib::SnPrintf(buf, size, "%s+0x%p", name, offset);
        return true;
    }

    return ModuleTable::GetInstance().Describe(addr, buf, size);
}

}
