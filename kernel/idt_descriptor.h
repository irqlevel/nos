#pragma once

#include <include/types.h>

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

    void SetOffset(u32 offset);
    void SetSelector(u16 selector);
    void SetType(u8 type);
    void SetPresent(bool on);
    void SetStorageSegment(bool on);
    void SetDpl(u8 dpl);

    u32 GetOffset() const;
    u16 GetSelector() const;
    u8 GetType() const;
    bool GetPresent() const;
    bool GetStorageSegment() const;
    u8 GetDpl() const;
    u64 GetValue() const;

    static IdtDescriptor Encode(bool present, bool ss, u8 dpl, u32 offset, u16 selector, u8 type);

    IdtDescriptor(IdtDescriptor&& other);
    IdtDescriptor(const IdtDescriptor& other);

    IdtDescriptor& operator=(IdtDescriptor&& other);
    IdtDescriptor& operator=(const IdtDescriptor& other);

    static IdtDescriptor Encode(void (*handlerFn)());

private:
    u64 Value;
    u64 HighValue;

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
}
