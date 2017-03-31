#include "idt.h"
#include "helpers32.h"
#include "stdlib.h"

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

u32 Idt::GetBase()
{
    return Base;
}

u16 Idt::GetLimit()
{
    return Limit;
}

IdtDescriptor Idt::GetDescriptor(u16 index)
{
	if (index * sizeof(u64) >= Limit)
		return IdtDescriptor(0);

    return IdtDescriptor(*(reinterpret_cast<u64*>(Base) + index));
}

void Idt::SetDescriptor(u16 index, const IdtDescriptor& desc)
{
	if (index * sizeof(u64) >= Limit)
		return;

    u64* base = reinterpret_cast<u64*>(Base) + index;

    *base = desc.GetValue();
    return;
}

}
}
