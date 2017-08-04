#include "gdt_descriptor.h"
#include "idt_descriptor.h"
#include "asm.h"

#include <lib/stdlib.h>

namespace Kernel
{

IdtDescriptor::IdtDescriptor()
    : Value(0)
    , HighValue(0)
{
}

IdtDescriptor::IdtDescriptor(u64 value)
    : Value(value)
    , HighValue(0)
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

void IdtDescriptor::SetOffset(u32 offset)
{
    Value |= static_cast<u64>(Stdlib::HighPart(offset)) << 48;
    Value |= Stdlib::LowPart(offset);
}

void IdtDescriptor::SetSelector(u16 selector)
{
    Value |= static_cast<u32>(selector) << 16;
}

void IdtDescriptor::SetType(u8 type)
{
    if (type & (1 << 3))
        Stdlib::SetBit(Value, 43);
    else
        Stdlib::ClearBit(Value, 43);

    if (type & (1 << 2))
        Stdlib::SetBit(Value, 42);
    else
        Stdlib::ClearBit(Value, 42);

    if (type & (1 << 1))
        Stdlib::SetBit(Value, 41);
    else
        Stdlib::ClearBit(Value, 41);

    if (type & (1 << 0))
        Stdlib::SetBit(Value, 40);
    else
        Stdlib::ClearBit(Value, 40);
}

void IdtDescriptor::SetPresent(bool on)
{
    if (on)
        Stdlib::SetBit(Value, 47);
    else
        Stdlib::ClearBit(Value, 47);
}

void IdtDescriptor::SetStorageSegment(bool on)
{
    if (on)
        Stdlib::SetBit(Value, 44);
    else
        Stdlib::ClearBit(Value, 44);
}

void IdtDescriptor::SetDpl(u8 dpl)
{
    if (dpl & (1 << 1))
        Stdlib::SetBit(Value, 46);
    else
        Stdlib::ClearBit(Value, 46);

    if (dpl & (1 << 0))
        Stdlib::SetBit(Value, 45);
    else
        Stdlib::ClearBit(Value, 45);
}

bool IdtDescriptor::GetPresent() const
{
    return (Stdlib::TestBit(Value, 47)) ? true : false;
}

bool IdtDescriptor::GetStorageSegment() const
{
    return (Stdlib::TestBit(Value, 44)) ? true : false;
}

u8 IdtDescriptor::GetDpl() const
{
    return (Stdlib::TestBit(Value, 46) << 1) | (Stdlib::TestBit(Value, 45) << 0);
}

u64 IdtDescriptor::GetValue() const
{
    return Value;
}

IdtDescriptor IdtDescriptor::Encode(bool present, bool ss, u8 dpl, u32 offset, u16 selector, u8 type)
{
    IdtDescriptor desc(0);

    desc.SetPresent(present);
    desc.SetStorageSegment(ss);
    desc.SetDpl(dpl);
    desc.SetOffset(offset);
    desc.SetSelector(selector);
    desc.SetType(type);

    return desc;
}

IdtDescriptor::IdtDescriptor(IdtDescriptor&& other)
{
    Value = other.Value;
    HighValue = other.HighValue;
    other.Value = 0;
    other.HighValue = 0;
}

IdtDescriptor::IdtDescriptor(const IdtDescriptor& other)
{
    Value = other.Value;
    HighValue = other.HighValue;
}

IdtDescriptor& IdtDescriptor::operator=(IdtDescriptor&& other)
{
    if (this != &other)
    {
        Value = other.Value;
        HighValue = other.HighValue;
        other.Value = 0;
        other.HighValue = 0;
    }
    return *this;
}

IdtDescriptor& IdtDescriptor::operator=(const IdtDescriptor& other)
{
    if (this != &other)
    {
        Value = other.Value;
        HighValue = other.HighValue;
    }
    return *this;
}

IdtDescriptor IdtDescriptor::Encode(void (*handlerFn)())
{
    if (handlerFn != nullptr)
    {
        return Encode(reinterpret_cast<u64>(handlerFn), GetCs(),
            IdtDescriptor::FlagPresent | IdtDescriptor::FlagGateInterrupt80386_32);
    }
    else
    {
        return IdtDescriptor(0);
    }
}

}
