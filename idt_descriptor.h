#pragma once

#include "types.h"

namespace Kernel
{

namespace Core
{


class IdtDescriptor final
{
public:
    IdtDescriptor();
    IdtDescriptor(u64 value);
    ~IdtDescriptor();

    u32 GetOffset();
    u16 GetSelector();
    u8 GetType();

    u64 GetValue();

    static IdtDescriptor Encode(u32 offset, u16 selector, u8 type);

    IdtDescriptor(IdtDescriptor&& other);
    IdtDescriptor(const IdtDescriptor& other);

    IdtDescriptor& operator=(IdtDescriptor&& other);
    IdtDescriptor& operator=(const IdtDescriptor& other);

    IdtDescriptor& operator=(void (*)(void*));

private:
    u64 Value;

    static const u8 FlagPresent = (1 << 7);
    static const u8 FlagDPL0 = 0;
    static const u8 FlagDPL1 = (1 << 5);
    static const u8 FlagDPL2 = (1 << 6);
    static const u8 FlagDPL3 = (1 << 5) | (1 << 6);
    static const u8 FlagGateTask80386_32 = 0x5;
    static const u8 FlagGateInterrupt80286_16 = 0x6;
    static const u8 FlagGateTrap80286_16 = 0x7;
    static const u8 FlagGateInterrupt80386_32 = 0xE;
    static const u8 FlagGateTrap80386_32 = 0xF;

    __attribute((interrupt))
    static void DummyHandler(void *);
};

}
}
