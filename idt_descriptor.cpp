#include "gdt_descriptor.h"
#include "idt_descriptor.h"
#include "helpers32.h"

namespace Kernel
{

namespace Core
{

IdtDescriptor::IdtDescriptor()
{
    *this = &IdtDescriptor::DummyHandler;
}

IdtDescriptor::IdtDescriptor(u64 value)
    : Value(value)
{
}

IdtDescriptor::~IdtDescriptor()
{
}

u32 IdtDescriptor::GetOffset()
{
        return ((Value >> 48) << 16) | (Value & 0xFFFF);
}

u16 IdtDescriptor::GetSelector()
{
        return (Value >> 16) & 0xFFFF;
}

u8 IdtDescriptor::GetType()
{
        return (Value >> 40) & 0xFF;
}

IdtDescriptor IdtDescriptor::Encode(u32 offset, u16 selector, u8 type)
{
	u64 value = 0;

        value |= (offset & 0xFFFF);
        value |= selector << 16;
        value |= ((u64) type) << 40;
        value |= ((u64) offset >> 16) << 48;

	return IdtDescriptor(value);
}

IdtDescriptor::IdtDescriptor(IdtDescriptor&& other)
{
    Value = other.Value;
    other.Value = 0;
}

IdtDescriptor::IdtDescriptor(const IdtDescriptor& other)
{
    Value = other.Value;
}

IdtDescriptor& IdtDescriptor::operator=(IdtDescriptor&& other)
{
    if (this != &other)
    {
        Value = other.Value;
        other.Value = 0;
    }
    return *this;
}

IdtDescriptor& IdtDescriptor::operator=(const IdtDescriptor& other)
{
    if (this != &other)
    {
        Value = other.Value;
    }
    return *this;
}

IdtDescriptor& IdtDescriptor::operator=(void (*fn)(void*))
{
    if (fn)
        *this = Encode((u32) fn, GdtDescriptor::GdtCode, IdtDescriptor::FlagPresent | IdtDescriptor::FlagGateInterrupt80386_32);
    else
        Value = 0;

    return *this;
}

void IdtDescriptor::DummyHandler(void* __attribute((unused)) frame)
{
    outb(0x20, 0x20);
}

}
}
