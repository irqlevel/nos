#include "idt.h"
#include "helpers32.h"
#include "stdlib.h"
#include "pic.h"

namespace Kernel
{

namespace Core
{

Idt::Idt()
    : Base(0)
    , Limit(0)
{
    for (size_t i = 0; i < Shared::ArraySize(Entry); i++)
    {
        Entry[i] = IdtDescriptor::Encode(DummyInterruptStub);
    }

    Save();
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

void Idt::Save()
{
    TableDesc desc = {
        .Base = (u32) &Entry[0],
        .Limit = sizeof(Entry),
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

IdtDescriptor Idt::GetDescriptor(u16 index)
{
    if (index > Shared::ArraySize(Entry))
	    return IdtDescriptor(0);

    return Entry[index];
}

void Idt::SetDescriptor(u16 index, const IdtDescriptor& desc)
{
    if (index > Shared::ArraySize(Entry))
	    return;

    Entry[index] = desc;
}

void Idt::DummyHandler()
{
    DummyHandlerCounter.Inc();
    Pic::EOI();
}

extern "C" void DummyInterrupt()
{
    auto& idt = Idt::GetInstance();

    idt.DummyHandler();
}

}
}
