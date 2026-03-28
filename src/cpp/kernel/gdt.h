#pragma once

#include "gdt_descriptor.h"
#include <lib/stdlib.h>

namespace Kernel
{

class Gdt final
{
public:
    static Gdt& GetInstance()
    {
        static Gdt Instance;
        return Instance;
    }

    void Save();

private:
    Gdt();
    ~Gdt();

    struct TableDesc {
	    u16 Limit;
	    u64 Base;
    } __attribute((packed));

    u64 Base;
    u16 Limit;

    GdtDescriptor Entry[2];
};

}