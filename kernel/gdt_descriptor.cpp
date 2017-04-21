#include "gdt_descriptor.h"

namespace Kernel
{

GdtDescriptor::GdtDescriptor()
	: Value(0)
{
}

GdtDescriptor::GdtDescriptor(u64 value)
    : Value(value)
{
}

GdtDescriptor::~GdtDescriptor()
{
}

u32 GdtDescriptor::GetBase()
{
	u32 high, low;
	u32 base;

	high = Shared::HighPart(Value);
	low = Shared::LowPart(Value);
	base = 0;
	base |= (low & 0xFFFF0000) >> 16;
	base |= (high & 0x000000FF) << 16;
	base |= (high & 0xFF000000);

	return base;
}

u32 GdtDescriptor::GetLimit()
{
	u32 high, low;
	u32 limit;

	high = Shared::HighPart(Value);
	low = Shared::LowPart(Value);
	limit = 0;
	limit |= (low & 0x0000FFFF);
	limit |= (high & 0x000F0000) << 16;
	return limit;
}

u8 GdtDescriptor::GetFlag()
{
	u32 high;

	high = Shared::HighPart(Value);
	return (high & 0x00F00000) >> 20;
}

u8 GdtDescriptor::GetAccess()
{
	u32 high;

	high = Shared::HighPart(Value);
	return (high & 0x0000FF00) >> 8;
}

u64 GdtDescriptor::GetValue()
{
    return Value;
}

void GdtDescriptor::SetValue(u64 value)
{
	Value = value;
}

GdtDescriptor GdtDescriptor::Encode(u32 base, u32 limit, u8 flag, u8 access)
{
	u64 value = 0;

	/* set high 32 bits */
	value |= (limit & 0x000F0000); /* limit bits 19:16 */
	value |= (flag & 0x0F) & 0x00F00000;
	value |= (access & 0xFF) & 0x000FF00;
	value |= (base >> 16) & 0x000000FF; /* base bits 23:16 */
	value |= base & 0xFF000000; /* base bits 32:24 */

	/* set low 32 bits */
	value <<= 32;
	value |= base << 16; /* base bits 15:0 */
	value |= limit & 0x0000FFFF; /* limit bits 15:0 */

	return GdtDescriptor(value);
}

GdtDescriptor::GdtDescriptor(GdtDescriptor&& other)
{
    Value = other.Value;
    other.Value = 0;
}

GdtDescriptor::GdtDescriptor(const GdtDescriptor& other)
{
    Value = other.Value;
}

GdtDescriptor& GdtDescriptor::operator=(GdtDescriptor&& other)
{
    if (this != &other)
    {
        Value = other.Value;
        other.Value = 0;
    }
    return *this;
}

GdtDescriptor& GdtDescriptor::operator=(const GdtDescriptor& other)
{
    if (this != &other)
    {
        Value = other.Value;
    }
    return *this;
}

}