#include "gdt.h"
#include "helpers32.h"
#include "memory.h"

namespace Kernel
{

namespace Core
{

Gdt::Gdt()
    : Base(0)
    , Limit(0)
{
}

Gdt::~Gdt()
{
}

void Gdt::Load()
{
    TableDesc desc;

    get_gdt_32(&desc);

    Base = desc.Base;
    Limit = desc.Limit;
}

u32 Gdt::GetBase()
{
    return Base;
}

u16 Gdt::GetLimit()
{
    return Limit;
}

GdtDescriptor Gdt::LoadDescriptor(u16 selector)
{
	if (selector & 7)
		return GdtDescriptor(0);
	if (selector == 0)
		return GdtDescriptor(0);
	if (selector >= Limit)
		return GdtDescriptor(0);

    u64* base = reinterpret_cast<u64*>(
        Shared::MemAdd(reinterpret_cast<void*>(Base), selector));

    return GdtDescriptor(*base);
}

}
}