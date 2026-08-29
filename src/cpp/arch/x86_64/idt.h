#pragma once

#include "idt_descriptor.h"
#include <kernel/atomic.h>

#include <include/types.h>

namespace Kernel
{

struct Context;

class Idt final
{
public:

    static Idt& GetInstance()
    {
        static Idt Instance;
        return Instance;
    }

    void Save();

    IdtDescriptor GetDescriptor(u16 index);

    void SetDescriptor(u16 index, const IdtDescriptor& desc);

    u32 GetBase();

    u16 GetLimit();

    void DummyInterrupt(Context* ctx, u8 vector);

private:
    Idt();
    ~Idt();

    Idt(const Idt& other) = delete;
    Idt(Idt&& other) = delete;

    Idt& operator=(const Idt& other) = delete;
    Idt& operator=(Idt&& other) = delete;

    /* Slots below this are CPU exceptions (Intel reserved); device
       interrupts start at 0x20 */
    static const u8 FirstDeviceVector = 0x20;

    /* Report a stray interrupt this many times, then only count it: a
       level-triggered source nobody can acknowledge repeats forever */
    static const long MaxDummyReports = 8;

    struct TableDesc {
	    u16 Limit;
	    u64 Base;
    } __attribute((packed));

    u64 Base;
    u16 Limit;

    IdtDescriptor Entry[256];
    Atomic DummyInterruptCounter;
};

}
