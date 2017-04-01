#pragma once

#include "types.h"
#include "idt_descriptor.h"
#include "atomic.h"

namespace Kernel
{

namespace Core
{

class Idt final
{
public:

    static Idt& GetInstance()
    {
        static Idt instance;
        return instance;
    }

    void Save();

    IdtDescriptor GetDescriptor(u16 index);

    void SetDescriptor(u16 index, const IdtDescriptor& desc);

    u32 GetBase();

    u16 GetLimit();

    void DummyHandler();

private:
    Idt();
    ~Idt();

    Idt(const Idt& other) = delete;
    Idt(Idt&& other) = delete;

    Idt& operator=(const Idt& other) = delete;
    Idt& operator=(Idt&& other) = delete;

    struct TableDesc {
	    u16 Limit;
	    u32 Base;
    } __attribute((packed));

    u32 Base;
    u16 Limit;

    IdtDescriptor Entry[256];
    Atomic DummyHandlerCounter;
};

}

}
