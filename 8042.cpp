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
}

void IO8042::Register(IdtDescriptor *_irq)
{
    irq = _irq;
    *irq = &IO8042::Interrupt;
}

void IO8042::Unregister()
{
    *irq = (void (*)(void*)) 0;
    irq = 0;
}

void IO8042::Interrupt(void __attribute((unused)) *frame)
{
	static IO8042& io8042 = IO8042::GetInstance();
        *io8042.BufPtr++ = inb(IO8042::Port);
        outb(0x20,0x20);
}

u8 IO8042::get()
{
        while (BufPtr == Buf)
                hlt();
        return *--BufPtr;
}

}
}
