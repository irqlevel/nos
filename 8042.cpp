#include "8042.h"
#include "memory_map.h"
#include "trace.h"
#include "helpers32.h"

namespace Kernel
{

namespace Core
{

IO8042::IO8042()
    : Buf(new u8[BufSize]),
      BufPtr(Buf)
{
}

IO8042::~IO8042()
{
    delete[] Buf;
}

void IO8042::Register(IdtDescriptor *irq)
{
    Irq = irq;
    *Irq = IO8042InterruptStub;
}

void IO8042::Unregister()
{
    *Irq = (void (*)())0;
    Irq = nullptr;
}

void IO8042::Interrupt()
{
    auto& io8042 = IO8042::GetInstance();

    *io8042.BufPtr++ = inb(Port);
    outb(0x20, 0x20);
}

u8 IO8042::Get()
{
        while (BufPtr == Buf)
                hlt();
        return *--BufPtr;
}

extern "C" void IO8042Interrupt()
{
    IO8042::Interrupt();
}

}
}
