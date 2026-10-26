#pragma once

#include <include/types.h>

namespace kernel
{

class IdtDescriptor final
{
public:
    IdtDescriptor();
    IdtDescriptor(u64 low_part, u64 high_part);

    ~IdtDescriptor();

    IdtDescriptor(IdtDescriptor&& other);
    IdtDescriptor(const IdtDescriptor& other);

    IdtDescriptor& operator=(IdtDescriptor&& other);
    IdtDescriptor& operator=(const IdtDescriptor& other);

    /* ist 1..7 selects a TSS IST stack for the gate; 0 = no stack switch */
    static IdtDescriptor encode(u64 offset, u16 selector, u8 type, u8 ist = 0);
    static IdtDescriptor encode(void (*handler_fn)(), u8 ist = 0);

private:
    u64 low_part_;
    u64 high_part_;

    static const u8 FLAG_PRESENT = (1 << 7);
    static const u8 FLAG_DPL0 = 0;
    static const u8 FLAG_DPL1 = (1 << 5);
    static const u8 FLAG_DPL2 = (1 << 6);
    static const u8 FLAG_DPL3 = (1 << 5) | (1 << 6);
    static const u8 FLAG_GATE_TASK80386_32 = 0x5;
    static const u8 FLAG_GATE_INTERRUPT80286_16 = 0x6;
    static const u8 FLAG_GATE_TRAP80286_16 = 0x7;
    static const u8 FLAG_GATE_INTERRUPT80386_32 = 0xE;
    static const u8 FLAG_GATE_TRAP80386_32 = 0xF;
};

}
