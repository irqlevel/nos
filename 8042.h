#pragma once

#include "types.h"
#include "idt_descriptor.h"

namespace Kernel
{

namespace Core
{

class IO8042
{
public:
    static IO8042& GetInstance()
    {
        static IO8042 instance;
        return instance;
    }

    void Register(IdtDescriptor *_irq);
    void Unregister();

    u8 get();

private:
    IO8042();
    virtual ~IO8042();

    IO8042(const IO8042& other) = delete;
    IO8042(IO8042&& other) = delete;
    IO8042& operator=(const IO8042& other) = delete;
    IO8042& operator=(IO8042&& other) = delete;

    static void Interrupt(void *frame);

    static const ulong Port = 0x60;

    const ulong BufSize = 16;
    volatile u8 *Buf;
    volatile u8 *BufPtr;
    IdtDescriptor *irq;
};

}
}
