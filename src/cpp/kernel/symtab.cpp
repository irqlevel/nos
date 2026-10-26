#include "symtab.h"
#include "module.h"

/* The kernel's text, from the linker script: only an address in it is one of
   Symbols[]' -- past the last function, a loadable module's code say, is not
   that function, however near the offset would come out */
extern "C" char kernel_start[], kernel_text[];

namespace kernel
{


SymbolTable::SymbolTable()
{
}

SymbolTable::~SymbolTable()
{
}

bool SymbolTable::resolve(ulong addr, const char*& name, ulong& offset)
{
    if (SYMBOL_COUNT == 0)
        return false;

    if (addr < reinterpret_cast<ulong>(kernel_start) || addr >= reinterpret_cast<ulong>(kernel_text))
        return false;

    /* Binary search for the largest Symbols[i].Addr <= addr */
    size_t lo = 0;
    size_t hi = SYMBOL_COUNT;

    while (lo < hi)
    {
        size_t mid = lo + (hi - lo) / 2;
        if (SYMBOLS[mid].addr <= addr)
            lo = mid + 1;
        else
            hi = mid;
    }

    if (lo == 0)
        return false;

    lo--;
    name = SYMBOLS[lo].name;
    offset = addr - SYMBOLS[lo].addr;
    return true;
}

bool SymbolTable::describe(ulong addr, char* buf, ulong size)
{
    const char* name;
    ulong offset;
    if (resolve(addr, name, offset))
    {
        stdlib::sn_printf(buf, size, "%s+0x%lX", name, offset);
        return true;
    }

    return ModuleTable::get_instance().describe(addr, false, buf, size);
}

bool SymbolTable::describe_return(ulong ret, char* buf, ulong size)
{
    if (ret == 0)
        return false;

    const char* name;
    ulong offset;
    if (resolve(ret - 1, name, offset))
    {
        stdlib::sn_printf(buf, size, "%s+0x%lX", name, offset + 1);
        return true;
    }

    return ModuleTable::get_instance().describe(ret, true, buf, size);
}

}
