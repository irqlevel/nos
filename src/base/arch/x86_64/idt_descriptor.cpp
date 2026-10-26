#include "gdt_descriptor.h"
#include "idt_descriptor.h"
#include "asm.h"

#include <lib/stdlib.h>

namespace kernel
{

IdtDescriptor::IdtDescriptor()
    : low_part_(0)
    , high_part_(0)
{
}

IdtDescriptor::IdtDescriptor(u64 low_part, u64 high_part)
    : low_part_(low_part)
    , high_part_(high_part)
{
}

IdtDescriptor::~IdtDescriptor()
{
}

IdtDescriptor::IdtDescriptor(IdtDescriptor&& other)
{
    high_part_ = other.high_part_;
    low_part_ = other.low_part_;
}

IdtDescriptor::IdtDescriptor(const IdtDescriptor& other)
{
    high_part_ = other.high_part_;
    low_part_ = other.low_part_;
}

IdtDescriptor& IdtDescriptor::operator=(IdtDescriptor&& other)
{
    if (this != &other)
    {
        high_part_ = other.high_part_;
        low_part_ = other.low_part_;
    }
    return *this;
}

IdtDescriptor& IdtDescriptor::operator=(const IdtDescriptor& other)
{
    if (this != &other)
    {
        high_part_ = other.high_part_;
        low_part_ = other.low_part_;
    }
    return *this;
}

IdtDescriptor IdtDescriptor::encode(u64 offset, u16 selector, u8 type, u8 ist)
{
	u64 low_part = 0, high_part = 0;

    low_part |= (offset & 0xFFFFULL);
    low_part |= selector << 16ULL;
    low_part |= ((u64)(ist & 0x7)) << 32ULL;
    low_part |= ((u64)type) << 40ULL;
    low_part |= ((offset >> 16ULL) & 0xFFFFULL) << 48ULL;
    high_part |= offset >> 32ULL;
	return IdtDescriptor(low_part, high_part);
}

IdtDescriptor IdtDescriptor::encode(void (*handler_fn)(), u8 ist)
{
    if (handler_fn != nullptr)
    {
        return encode(reinterpret_cast<u64>(handler_fn), get_cs(),
            IdtDescriptor::FLAG_PRESENT | IdtDescriptor::FLAG_GATE_INTERRUPT80386_32,
            ist);
    }
    else
    {
        return IdtDescriptor();
    }
}

}
