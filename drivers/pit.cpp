#include "pit.h"
#include "lapic.h"

#include <kernel/cpu.h>

namespace Kernel
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
    Stdlib::AutoLock lock(Lock);

    ReloadValue = 11932; // 1193182 / 11932.0 = 99.99849145155883
    TickMs = 10; // 1000 / 99.99849145155883 = 10.00015085711987
    TickMsNs = 150857;
    TimeMs = 0;
    TimeMsNs = 0;

    Outb(ModePort, 0b00110100); //channel 0, lobyte/hibyte, rate generator
    Outb(Channel0Port, Stdlib::LowPart(ReloadValue));
    Outb(Channel0Port, Stdlib::HighPart(ReloadValue));
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

void Pit::Interrupt(Context* ctx)
{
    (void)ctx;
    {
        Stdlib::AutoLock lock(Lock);

        TimeMs = TimeMs + TickMs;
        TimeMsNs = TimeMsNs + TickMsNs;
        while (TimeMsNs >= Const::NanoSecsInMs)
        {
            TimeMsNs = TimeMsNs - Const::NanoSecsInMs;
            TimeMs = TimeMs + 1;
        }
    }

    CpuTable::GetInstance().SendIPIAll();
    Lapic::EOI(IntVector);
}

Stdlib::Time Pit::GetTime()
{
    return Stdlib::Time(TimeMs * Const::NanoSecsInMs + TimeMsNs);
}

extern "C" void PitInterrupt(Context* ctx)
{
    Pit::GetInstance().Interrupt(ctx);
}

void Pit::Wait(const Stdlib::Time& timeout)
{
    Stdlib::Time expired = GetTime() + timeout;

    while (GetTime() < expired)
    {
        Pause();
    }
}

void Pit::Wait(ulong nanoSecs)
{
    Stdlib::Time timeout(nanoSecs);

    Wait(timeout);
}

}