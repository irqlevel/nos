#include "8042.h"
#include "memory_map.h"
#include "trace.h"
#include "asm.h"
#include "pic.h"
#include "vga.h"
#include "lapic.h"

namespace Kernel
{

namespace Core
{

IO8042::IO8042()
    : IntVector(-1)
    , Mod(0)
{
    for (size_t i = 0; i < Shared::ArraySize(Observer); i++)
        Observer[i] = nullptr;
}

IO8042::~IO8042()
{
}

void IO8042::OnInterruptRegister(u8 irq, u8 vector)
{
    (void)irq;

    IntVector = vector;

    Shared::Time period;

    period.Secs = 0;
    period.NanoSecs = 10 * 1000 * 1000; // 10ms

    TimerTable::GetInstance().StartTimer(*this, period);
}

InterruptHandlerFn IO8042::GetHandlerFn()
{
    return IO8042InterruptStub;
}

void IO8042::Interrupt()
{
    Shared::AutoLock lock(Lock);

    if (!Buf.Put(Inb(Port)))
    {
        Trace(0, "Kbd: can't put new code");
    }

    Lapic::GetInstance().EOI(IntVector);
}


bool IO8042::RegisterObserver(IO8042Observer& observer)
{
    Shared::AutoLock lock(Lock);

    for (size_t i = 0; i < MaxObserver; i++)
    {
        if (Observer[i] == nullptr)
        {
            Observer[i] = &observer;
            return true;
        }
    }

    return false;
}

void IO8042::UnregisterObserver(IO8042Observer& observer)
{
    Shared::AutoLock lock(Lock);

    for (size_t i = 0; i < MaxObserver; i++)
    {
        if (Observer[i] == &observer)
        {
            Observer[i] = nullptr;
        }
    }
}

void IO8042::OnTick(TimerCallback& callback)
{
    (void)callback;

    Shared::AutoLock lock(Lock);

    while (!Buf.IsEmpty())
    {
        static char map[0x80] = "__1234567890-=_" "\tqwertyuiop[]\n" "_asdfghjkl;'`" "_\\zxcvbnm,./_" "*_ _";
        u8 code = Buf.Get();

        Trace(KbdLL, "Kbd: code 0x%p", (ulong)code);

        if (code == 0x2a || code == 0x36) Mod = 0x20;
        else if (code == 0xaa || code == 0xb6) Mod = 0x00;
        else if (code & 0x80) continue;
        else
        {
            char c = map[(int)code] ^ Mod;

            Trace(KbdLL, "Kbd: char %c", c);

            for (size_t i = 0; i < MaxObserver; i++)
            {
                if (Observer[i] != nullptr)
                    Observer[i]->OnChar(c);
            }
        }
    }
}

extern "C" void IO8042Interrupt()
{
    IO8042::GetInstance().Interrupt();
}

}
}
