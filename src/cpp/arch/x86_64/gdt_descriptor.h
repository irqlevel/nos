#pragma once

#include <lib/stdlib.h>

namespace kernel
{

class GdtDescriptor final
{
public:
    GdtDescriptor();
    GdtDescriptor(u64 value);
    ~GdtDescriptor();

    u32 get_base();
    u32 get_limit();
    u8 get_flag();
    u8 get_access();

    u64 get_value();

    void set_value(u64 value);

    static GdtDescriptor encode(u32 base, u32 limit, u8 flag, u8 access);

    GdtDescriptor(GdtDescriptor&& other);
    GdtDescriptor(const GdtDescriptor& other);

    GdtDescriptor& operator=(GdtDescriptor&& other);
    GdtDescriptor& operator=(const GdtDescriptor& other);

private:
    u64 value_;

    static const u8 FLAG_GRANULARITY = (1 << 3);
    static const u8 FLAG_SIZE = (1 << 2);

    static const u8 ACCESS_PRESENCE = (1 << 7);
    static const u8 ACCESS_PRIVL1 = (1 << 6);
    static const u8 ACCESS_PRIVL0 = (1 << 5);
    static const u8 ACCESS_ALW_ON_BIT4 = (1 << 4);
    static const u8 ACCESS_EXEC = (1 << 3);
    static const u8 ACCESS_DC = (1 << 2);
    static const u8 ACCESS_RW = (1 << 1);
    static const u8 ACCESS_AC = (1 << 0);
    static const u8 ACCESS_KERNEL = 0;
    static const u8 ACCESS_USER = (ACCESS_PRIVL1|ACCESS_PRIVL0);

};

}
