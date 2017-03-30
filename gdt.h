#pragma once

#include "types.h"
#include "gdt_descriptor.h"

namespace Kernel
{

namespace Core
{

class Gdt final
{
public:
    Gdt();
    ~Gdt();
    void Load();

    GdtDescriptor LoadDescriptor(u16 selector);

    u32 GetBase();

    u16 GetLimit();

private:
    struct TableDesc {
	    u16 Limit;
	    u32 Base;
    } __attribute((packed));

    u32 Base;
    u16 Limit;
};

}

}