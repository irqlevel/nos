#include "helpers32.h"
#include "stdlib.h"
#include "idt.h"
#include "pit.h"
#include "pic.h"
#include "stdlib.h"

namespace Kernel
{

namespace Core
{

Pit::Pit()
    : IntNum(-1)
    , TimeMs(0)
    , TimeMsNs(0)
    , TickMs(0)
    , TickMsNs(0)
    , ReloadValue(0)
{
}

Pit::~Pit()
{
}

void Pit::Setup()
{
    ReloadValue = 11932; // 1193182 / 11932.0 = 99.99849145155883
    TickMs = 10; // 1000 / 99.99849145155883 = 10.00015085711987
    TickMsNs = 150857;
    TimeMs = 0;
    TimeMsNs = 0;

    outb(ModePort, 0b00110100); //channel 0, lobyte/hibyte, rate generator
    outb(Channel0Port, Shared::LowPart(ReloadValue));
    outb(Channel0Port, Shared::HighPart(ReloadValue));
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

    TimeMs += TickMs;
    TimeMsNs += TickMsNs;
    while (TimeMsNs >= 1000000)
    {
        TimeMsNs -= 1000000;
        TimeMs += 1;
    }

    Pic::EOI();
}

Shared::Time Pit::GetTime()
{
    Shared::Time time;

    time.Secs = TimeMs / 1000;
    time.NanoSecs = 1000000 * (TimeMs % 1000) + TimeMsNs;

    return time;
}

extern "C" void PitInterrupt()
{
    auto& pit = Pit::GetInstance();
    pit.Interrupt();
}

}
}