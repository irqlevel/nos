#include "asm.h"
#include "stdlib.h"
#include "idt.h"
#include "pit.h"
#include "pic.h"
#include "stdlib.h"
#include "timer.h"
#include "lapic.h"

namespace Kernel
{

namespace Core
{

Pit::Pit()
    : IntVector(-1)
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

    Outb(ModePort, 0b00110100); //channel 0, lobyte/hibyte, rate generator
    Outb(Channel0Port, Shared::LowPart(ReloadValue));
    Outb(Channel0Port, Shared::HighPart(ReloadValue));
}

void Pit::OnInterruptRegister(u8 irq, u8 vector)
{
    (void)irq;
    IntVector = vector;
}

InterruptHandlerFn Pit::GetHandlerFn()
{
    return PitInterruptStub;
}

void Pit::Interrupt()
{
    TimeMs += TickMs;
    TimeMsNs += TickMsNs;
    while (TimeMsNs >= 1000000)
    {
        TimeMsNs -= 1000000;
        TimeMs += 1;
    }

    TimerTable::GetInstance().ProcessTimers();

    Lapic::GetInstance().EOI(IntVector);
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
    Pit::GetInstance().Interrupt();
}

}
}