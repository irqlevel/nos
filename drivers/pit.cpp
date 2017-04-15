#include "pit.h"
#include "pic.h"
#include "lapic.h"

#include <kernel/asm.h>
#include <kernel/idt.h>
#include <kernel/timer.h>
#include <kernel/trace.h>
#include <kernel/cpu.h>

#include <lib/stdlib.h>

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
    Shared::AutoLock lock(Lock);

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

void Pit::Interrupt(Context* ctx)
{
    (void)ctx;

    {
        Shared::AutoLock lock(Lock);

        TimeMs += TickMs;
        TimeMsNs += TickMsNs;
        while (TimeMsNs >= NanoSecsInMs)
        {
            TimeMsNs -= NanoSecsInMs;
            TimeMs += 1;
        }
    }

    Lapic::EOI(IntVector);

    TimerTable::GetInstance().ProcessTimers();

    auto& cpu = CpuTable::GetInstance().GetCurrentCpu();
    //ask other cpu's to schedule tasks
    CpuTable::GetInstance().SendIPIAllExclude(cpu.GetIndex());
    //and self task scheduling
    cpu.Schedule();
}

Shared::Time Pit::GetTime()
{
    Shared::AutoLock lock(Lock);

    return TimeMs * NanoSecsInMs + TimeMsNs;
}

extern "C" void PitInterrupt(Context* ctx)
{
    Pit::GetInstance().Interrupt(ctx);
}

void Pit::Wait(const Shared::Time& timeout)
{
    Shared::Time expired = GetTime() + timeout;

    while (GetTime() < expired)
    {
        Hlt();
    }
}

void Pit::Wait(ulong nanoSecs)
{
    Shared::Time timeout(nanoSecs);

    Wait(timeout);
}

}