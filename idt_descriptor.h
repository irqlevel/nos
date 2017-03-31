#pragma once

#include "types.h"

namespace Kernel
{

namespace Core
{


class IdtDescriptor final
{
public:
    IdtDescriptor(u64 value);
    ~IdtDescriptor();

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

private:
    u64 Value;

    struct Content {
        u16 OffsetLow;
        u16 Selector;
        u8 Zero;
        u8 Type;
        u16 OffsetHigh;
    } __attribute((packed));

};

}
}
