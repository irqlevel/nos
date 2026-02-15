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

    bool Resolve(ulong addr, const char*& name, ulong& offset);

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
