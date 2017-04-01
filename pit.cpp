#include "helpers32.h"
#include "stdlib.h"
#include "idt.h"
#include "pit.h"
#include "pic.h"

namespace Kernel
{

namespace Core
{

Pit::Pit()
    : IntNum(-1)
{
}


Pit::~Pit()
{
}

void Pit::RegisterInterrupt(int intNum)
{
    auto& idt = Idt::GetInstance();

    idt.SetDescriptor(intNum, IdtDescriptor::Encode(PitInterruptStub));
    IntNum = intNum;
}

void Pit::UnregisterInterrupt()
{
    if (IntNum >= 0)
    {
        auto& idt = Idt::GetInstance();

        idt.SetDescriptor(IntNum, IdtDescriptor(0));
        IntNum = -1;
    }
}

void Pit::Interrupt()
{
    IntCounter.Inc();

    Pic::EOI();
}

extern "C" void PitInterrupt()
{
    auto& pit = Pit::GetInstance();
    pit.Interrupt();
}

}
}