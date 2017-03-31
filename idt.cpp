#include "idt.h"
#include "helpers32.h"
#include "memory.h"

namespace Kernel
{

namespace Core
{

Idt::Idt()
    : Base(0)
    , Limit(0)
{
}

Idt::~Idt()
{
}

void Idt::Load()
{
    TableDesc desc;

    get_idt_32(&desc);

    Base = desc.Base;
    Limit = desc.Limit;
}

void Idt::Save(const IdtDescriptor* base, u16 length)
{
    TableDesc desc = {
        .Base = (u32) base,
        .Limit = length,
    };

    put_idt_32(&desc);

    Base = desc.Base;
    Limit = desc.Limit;
}

u32 Idt::GetBase()
{
    return Base;
}

u16 Idt::GetLimit()
{
    return Limit;
}

IdtDescriptor Idt::LoadDescriptor(u16 irq)
{
    u32 selector = irq << 3;
    if (selector >= Limit)
	return IdtDescriptor(0);

    u64* base = reinterpret_cast<u64*>(
        Shared::MemAdd(reinterpret_cast<void*>(Base), selector));

    return IdtDescriptor(*base);
}

}
}
