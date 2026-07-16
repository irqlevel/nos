#include "8042.h"
#include <arch/x86_64/pic.h>
#include "vga.h"
#include <arch/x86_64/lapic.h>

#include <kernel/trace.h>
#include <kernel/asm.h>
#include <kernel/interrupt.h>
#include <mm/memory_map.h>

namespace Kernel
{

IO8042::IO8042()
    : IntVector(-1)
    , Mod(0)
    , Extended(0)
{
    Trace(0, "IO8042 0x%p status 0x%p", this, (ulong)Inb(StatusPort));

    VgaTerm::GetInstance().Printf("IO8042 status 0x%p\n", (ulong)Inb(StatusPort));

    for (size_t i = 0; i < Stdlib::ArraySize(Observer); i++)
        Observer[i] = nullptr;

    ReadData();
}

IO8042::~IO8042()
{
}

void IO8042::OnInterruptRegister(u8 irq, u8 vector)
{
    (void)irq;

    IntVector = vector;

    Stdlib::Time period(10 * Const::NanoSecsInMs); //10ms

    TimerTable::GetInstance().StartTimer(*this, period);
}

InterruptHandlerFn IO8042::GetHandlerFn()
{
    return IO8042InterruptStub;
}

void IO8042::ReadData()
{
    Stdlib::AutoLock lock(Lock);

    /* Bounded drain: a stuck status bit must not hang the boot or the ISR */
    for (size_t i = 0; i < MaxDrainIterations; i++)
    {
        if (!(Inb(StatusPort) & 0x1))
            break;

        if (!Buf.Put(Inb(DataPort)))
        {
            Trace(0, "Kbd: can't put new code");
        }
    }
}

void IO8042::Interrupt(Context* ctx)
{
    (void)ctx;

    InterruptCounter.Inc();
    ReadData();
    Lapic::EOI(IntVector);
}


bool IO8042::RegisterObserver(IO8042Observer& observer)
{
    Stdlib::AutoLock lock(Lock);

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
    Stdlib::AutoLock lock(Lock);

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

    Stdlib::AutoLock lock(Lock);

    while (!Buf.IsEmpty())
    {
        static char map[0x80] = "__1234567890-=_" "\tqwertyuiop[]\n" "_asdfghjkl;'`" "_\\zxcvbnm,./_" "*_ _";
        static char shiftMap[0x80] = "__!@#$%^&*()_+_" "\tQWERTYUIOP{}\n" "_ASDFGHJKL:\"~" "_|ZXCVBNM<>?_" "*_ _";
        u8 code = Buf.Get();

        Trace(KbdLL, "Kbd: code 0x%p", (ulong)code);

        if (code == 0xE0)
        {
            /* Extended-key prefix: the next make/break code is an extended
               scancode (arrows, nav keys, right-side modifiers). */
            Extended = 1;
            continue;
        }

        if (Extended)
        {
            /* Ignore the extended code rather than misdecoding it into a
               bogus character (or a NUL) via the base scancode map. */
            Extended = 0;
            continue;
        }

        if (code == 0x2a || code == 0x36) Mod = 1;
        else if (code == 0xaa || code == 0xb6) Mod = 0;
        else if (code & 0x80) continue;
        else
        {
            BugOn(code >= Stdlib::ArraySize(map));

            char c = Mod ? shiftMap[code] : map[code];
            if (c == '\0')
                continue; /* unmapped key: never deliver a NUL to observers */

            Trace(KbdLL, "Kbd: char %c", c);

            for (size_t i = 0; i < MaxObserver; i++)
            {
                if (Observer[i] != nullptr)
                    Observer[i]->OnChar(c, code);
            }
        }
    }
}

extern "C" void IO8042Interrupt(Context* ctx)
{
    InterruptStats::Inc(IrqIO8042);
    IO8042::GetInstance().Interrupt(ctx);
}

}
