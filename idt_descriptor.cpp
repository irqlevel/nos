#include "idt_descriptor.h"
#include "stdlib.h"

namespace Kernel
{

namespace Core
{

IdtDescriptor::IdtDescriptor(u64 value)
    : Value(value)
{
}

IdtDescriptor::~IdtDescriptor()
{
}

void IdtDescriptor::SetOffset(u32 offset)
{
    Value |= static_cast<u64>(U32_HIGH(offset)) << 48;
    Value |= U32_LOW(offset);
}

void IdtDescriptor::SetSelector(u16 selector)
{
    Value |= static_cast<u32>(selector) << 16;
}

void IdtDescriptor::SetType(u8 type)
{
    if (type & (1 << 3))
        Shared::SetBit(Value, 43);
    else
        Shared::ClearBit(Value, 43);

    if (type & (1 << 2))
        Shared::SetBit(Value, 42);
    else
        Shared::ClearBit(Value, 42);

    if (type & (1 << 1))
        Shared::SetBit(Value, 41);
    else
        Shared::ClearBit(Value, 41);

    if (type & (1 << 0))
        Shared::SetBit(Value, 40);
    else
        Shared::ClearBit(Value, 40);
}

void IdtDescriptor::SetPresent(bool on)
{
    if (on)
        Shared::SetBit(Value, 47);
    else
        Shared::ClearBit(Value, 47);
}

void IdtDescriptor::SetStorageSegment(bool on)
{
    if (on)
        Shared::SetBit(Value, 44);
    else
        Shared::ClearBit(Value, 44);
}

void IdtDescriptor::SetDpl(u8 dpl)
{
    if (dpl & (1 << 1))
        Shared::SetBit(Value, 46);
    else
        Shared::ClearBit(Value, 46);

    if (dpl & (1 << 0))
        Shared::SetBit(Value, 45);
    else
        Shared::ClearBit(Value, 45);
}

u32 IdtDescriptor::GetOffset() const
{
    return (U64_HIGH(Value) & 0xFFFF0000) | (U64_LOW(Value) & 0x0000FFFF);
}

u16 IdtDescriptor::GetSelector() const
{
    return (U64_LOW(Value) & 0xFFFF0000) >> 16;
}

u8 IdtDescriptor::GetType() const
{
    return (Shared::TestBit(Value, 43) << 3) | (Shared::TestBit(Value, 42) << 2) |
        (Shared::TestBit(Value, 41) << 1) | (Shared::TestBit(Value, 40) << 0);
}

bool IdtDescriptor::GetPresent() const
{
    return (Shared::TestBit(Value, 47)) ? true : false;
}

bool IdtDescriptor::GetStorageSegment() const
{
    return (Shared::TestBit(Value, 44)) ? true : false;
}

u8 IdtDescriptor::GetDpl() const
{
    return (Shared::TestBit(Value, 46) << 1) | (Shared::TestBit(Value, 45) << 0);
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

}
}
