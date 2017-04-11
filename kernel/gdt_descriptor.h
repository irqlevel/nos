#pragma once

#include <include/types.h>

namespace Kernel
{

namespace Core
{


class GdtDescriptor final
{
public:
    GdtDescriptor(u64 value);
    ~GdtDescriptor();

    u32 GetBase();
    u32 GetLimit();
    u8 GetFlag();
    u8 GetAccess();

    u64 GetValue();

    static GdtDescriptor Encode(u32 base, u32 limit, u8 flag, u8 access);

    GdtDescriptor(GdtDescriptor&& other);
    GdtDescriptor(const GdtDescriptor& other);

    GdtDescriptor& operator=(GdtDescriptor&& other);
    GdtDescriptor& operator=(const GdtDescriptor& other);

private:
    u64 Value;

    static const u8 FlagGranularity = (1 << 3);
    static const u8 FlagSize = (1 << 2);

    static const u8 AccessPresence = (1 << 7);
    static const u8 AccessPrivl1 = (1 << 6);
    static const u8 AccessPrivl0 = (1 << 5);
    static const u8 AccessAlwOnBit4 = (1 << 4);
    static const u8 AccessExec = (1 << 3);
    static const u8 AccessDc = (1 << 2);
    static const u8 AccessRw = (1 << 1);
    static const u8 AccessAc = (1 << 0);
    static const u8 AccessKernel = 0;
    static const u8 AccessUser = (AccessPrivl1|AccessPrivl0);

};

}
}
