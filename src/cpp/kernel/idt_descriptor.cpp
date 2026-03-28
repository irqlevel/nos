#include "gdt_descriptor.h"
#include "idt_descriptor.h"
#include "asm.h"

#include <lib/stdlib.h>

namespace Kernel
{

IdtDescriptor::IdtDescriptor()
    : LowPart(0)
    , HighPart(0)
{
}

IdtDescriptor::IdtDescriptor(u64 lowPart, u64 highPart)
    : LowPart(lowPart)
    , HighPart(highPart)
{
}

IdtDescriptor::~IdtDescriptor()
{
}

IdtDescriptor::IdtDescriptor(IdtDescriptor&& other)
{
    HighPart = other.HighPart;
    LowPart = other.LowPart;
}

IdtDescriptor::IdtDescriptor(const IdtDescriptor& other)
{
    HighPart = other.HighPart;
    LowPart = other.LowPart;
}

IdtDescriptor& IdtDescriptor::operator=(IdtDescriptor&& other)
{
    if (this != &other)
    {
        HighPart = other.HighPart;
        LowPart = other.LowPart;
    }
    return *this;
}

IdtDescriptor& IdtDescriptor::operator=(const IdtDescriptor& other)
{
    if (this != &other)
    {
        HighPart = other.HighPart;
        LowPart = other.LowPart;
    }
    return *this;
}

IdtDescriptor IdtDescriptor::Encode(u64 offset, u16 selector, u8 type)
{
	u64 lowPart = 0, highPart = 0;

    lowPart |= (offset & 0xFFFFULL);
    lowPart |= selector << 16ULL;
    lowPart |= ((u64)type) << 40ULL;
    lowPart |= ((offset >> 16ULL) & 0xFFFFULL) << 48ULL;
    highPart |= offset >> 32ULL;
	return IdtDescriptor(lowPart, highPart);
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
        return IdtDescriptor();
    }
}

}
