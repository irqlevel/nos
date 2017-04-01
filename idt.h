#pragma once

#include "types.h"
#include "idt_descriptor.h"

namespace Kernel
{

namespace Core
{

class Idt final
{
public:
    Idt();
    ~Idt();
    void Load();

    void Save(const IdtDescriptor* base, u16 length);

    IdtDescriptor LoadDescriptor(u16 selector);

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
