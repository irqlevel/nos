#include "gdt_descriptor.h"

namespace kernel
{

GdtDescriptor::GdtDescriptor()
	: value_(0)
{
}

GdtDescriptor::GdtDescriptor(u64 value)
    : value_(value)
{
}

GdtDescriptor::~GdtDescriptor()
{
}

u32 GdtDescriptor::get_base()
{
	u32 high, low;
	u32 base;

	high = stdlib::high_part(value_);
	low = stdlib::low_part(value_);
	base = 0;
	base |= (low & 0xFFFF0000) >> 16;
	base |= (high & 0x000000FF) << 16;
	base |= (high & 0xFF000000);

	return base;
}

u32 GdtDescriptor::get_limit()
{
	u32 high, low;
	u32 limit;

	high = stdlib::high_part(value_);
	low = stdlib::low_part(value_);
	limit = 0;
	limit |= (low & 0x0000FFFF);
	limit |= (high & 0x000F0000) << 16;
	return limit;
}

u8 GdtDescriptor::get_flag()
{
	u32 high;

	high = stdlib::high_part(value_);
	return (high & 0x00F00000) >> 20;
}

u8 GdtDescriptor::get_access()
{
	u32 high;

	high = stdlib::high_part(value_);
	return (high & 0x0000FF00) >> 8;
}

u64 GdtDescriptor::get_value()
{
    return value_;
}

void GdtDescriptor::set_value(u64 value)
{
	value_ = value;
}

GdtDescriptor GdtDescriptor::encode(u32 base, u32 limit, u8 flag, u8 access)
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
    value_ = other.value_;
    other.value_ = 0;
}

GdtDescriptor::GdtDescriptor(const GdtDescriptor& other)
{
    value_ = other.value_;
}

GdtDescriptor& GdtDescriptor::operator=(GdtDescriptor&& other)
{
    if (this != &other)
    {
        value_ = other.value_;
        other.value_ = 0;
    }
    return *this;
}

GdtDescriptor& GdtDescriptor::operator=(const GdtDescriptor& other)
{
    if (this != &other)
    {
        value_ = other.value_;
    }
    return *this;
}

}