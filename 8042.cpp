#include "8042.h"
#include "memory_map.h"
#include "trace.h"
#include "helpers32.h"
#include "pic.h"

namespace Kernel
{

namespace Core
{

IO8042::IO8042()
    : Buf(new u8[BufSize])
    , BufPtr(Buf)
    , IntNum(-1)
{
}

IO8042::~IO8042()
{
    delete[] Buf;
}

void IO8042::RegisterInterrupt(int intNum)
{
    auto& idt = Idt::GetInstance();

    idt.SetDescriptor(intNum, IdtDescriptor::Encode(IO8042InterruptStub));
    IntNum = intNum;
}

void IO8042::UnregisterInterrupt()
{
    if (IntNum >= 0)
    {
        auto& idt = Idt::GetInstance();

        idt.SetDescriptor(IntNum, IdtDescriptor::Encode(nullptr));
        IntNum = -1;
    }
}

void IO8042::Interrupt()
{
    auto& io8042 = IO8042::GetInstance();

    *io8042.BufPtr++ = inb(Port);
    Pic::EOI();
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
