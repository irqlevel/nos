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

    /* Describe, for a return address -- what a backtrace's frames are: the
       function is the one holding ret - 1, the call's own last byte. A call
       to a function that does not return is its caller's last instruction,
       so the address after it is the next function's first, and naming ret
       itself put every frame of a panic under the function that happened
       to follow it in the image, at +0x0. The offset printed is ret's, just
       past the call, as before. */
    bool DescribeReturn(ulong ret, char* buf, ulong size);

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
