#pragma once

#include "idt_descriptor.h"
#include "atomic.h"

#include <include/types.h>

namespace Kernel
{

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

    void DummyInterrupt();

private:
    Idt();
    ~Idt();

    Idt(const Idt& other) = delete;
    Idt(Idt&& other) = delete;

    Idt& operator=(const Idt& other) = delete;
    Idt& operator=(Idt&& other) = delete;

    struct TableDesc {
	    u16 Limit;
	    u64 Base;
    } __attribute((packed));

    u64 Base;
    u16 Limit;

    IdtDescriptor Entry[256];
    Atomic DummyHandlerCounter;
};

}
