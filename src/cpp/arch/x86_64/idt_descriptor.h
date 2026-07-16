#pragma once

#include <include/types.h>

namespace Kernel
{

class IdtDescriptor final
{
public:
    IdtDescriptor();
    IdtDescriptor(u64 lowPart, u64 highPart);

    ~IdtDescriptor();

    IdtDescriptor(IdtDescriptor&& other);
    IdtDescriptor(const IdtDescriptor& other);

    IdtDescriptor& operator=(IdtDescriptor&& other);
    IdtDescriptor& operator=(const IdtDescriptor& other);

    /* ist 1..7 selects a TSS IST stack for the gate; 0 = no stack switch */
    static IdtDescriptor Encode(u64 offset, u16 selector, u8 type, u8 ist = 0);
    static IdtDescriptor Encode(void (*handlerFn)(), u8 ist = 0);

private:
    u64 LowPart;
    u64 HighPart;

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
};

}
