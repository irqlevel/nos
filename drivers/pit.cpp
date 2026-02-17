#include "pit.h"
#include "lapic.h"

#include <kernel/cpu.h>
#include <kernel/interrupt.h>

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
    /*
     * Reload = 11932 → actual rate = 1193182 / 11932 = 99.99849... Hz
     * Period = 10.000150857... ms = 10 ms + 151 ns
     */
    ReloadValue = DesiredReload;
    TickMs = 1000 / DesiredHz;
    /* Sub-ms nanosecond remainder per tick (rounded) */
    u64 periodNs = (u64)DesiredReload * Const::NanoSecsInSec;
    u64 wholeMsNs = (u64)TickMs * Const::NanoSecsInMs * BaseFrequency;
    TickMsNs = (ulong)((periodNs - wholeMsNs + BaseFrequency / 2) / BaseFrequency);
    TimeMs = 0;
    TimeMsNs = 0;

    Outb(ModePort, ModeCh0RateGen);
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
        TimeLock.WriteBegin();

        TimeMs = TimeMs + TickMs;
        TimeMsNs = TimeMsNs + TickMsNs;
        while (TimeMsNs >= Const::NanoSecsInMs)
        {
            TimeMsNs = TimeMsNs - Const::NanoSecsInMs;
            TimeMs = TimeMs + 1;
        }

        TimeLock.WriteEnd();
    }

    CpuTable::GetInstance().SendIPIAll();
    Lapic::EOI(IntVector);
}

Stdlib::Time Pit::GetTime()
{
    ulong ms, msNs;
    long seq;
    do {
        seq = TimeLock.ReadBegin();
        ms = TimeMs;
        msNs = TimeMsNs;
    } while (TimeLock.ReadRetry(seq));

    return Stdlib::Time(ms * Const::NanoSecsInMs + msNs);
}

extern "C" void PitInterrupt(Context* ctx)
{
    InterruptStats::Inc(IrqPit);
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