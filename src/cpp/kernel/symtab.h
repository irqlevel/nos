#pragma once

#include <lib/stdlib.h>

namespace Kernel
{

struct SymEntry
{
    ulong Addr;
    const char* Name;
};

class SymbolTable
{
public:
    static SymbolTable& GetInstance()
    {
        static SymbolTable Instance;

        return Instance;
    }

    /* The kernel's own function addr is in, and addr's offset into it; addr
       has to be in the kernel's text */
    bool Resolve(ulong addr, const char*& name, ulong& offset);

    /* addr as "name+0xoff" into buf: in a function of the kernel's, or of a
       loaded module's -- "name+0xoff [module]", copied, since the module
       may go the moment this returns. False, buf untouched, if addr is in
       neither. Never waits, so a panic may call it as well as bt. */
    bool Describe(ulong addr, char* buf, ulong size);

    /* Room for what Describe writes before it truncates */
    static const ulong DescribeMax = 160;

private:
    SymbolTable();
    ~SymbolTable();
    SymbolTable(const SymbolTable& other) = delete;
    SymbolTable(SymbolTable&& other) = delete;
    SymbolTable& operator=(const SymbolTable& other) = delete;
    SymbolTable& operator=(SymbolTable&& other) = delete;

    static const SymEntry Symbols[];
    static const size_t SymbolCount;
};

}
