#include "8042.h"
#include <arch/x86_64/pic.h>
#include "screen.h"
#include <hal/irqchip.h>

#include <kernel/trace.h>
#include <arch/x86_64/asm.h>
#include <kernel/interrupt.h>
#include <mm/memory_map.h>

namespace Kernel
{

IO8042::IO8042()
    : IntVector(-1)
    , Mod(0)
    , Ctrl(0)
    , Extended(0)
{
    Trace(0, "IO8042 0x%p status 0x%lX", this, (ulong)Inb(StatusPort));

    Screen::Printf("IO8042 status 0x%lX\n", (ulong)Inb(StatusPort));

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
    Hal::IrqEoi(IntVector);
}


const char* IO8042::ArrowSequence(u8 code)
{
    switch (code)
    {
    case CodeUp:
        return "\x1b[A";
    case CodeDown:
        return "\x1b[B";
    case CodeRight:
        return "\x1b[C";
    case CodeLeft:
        return "\x1b[D";
    default:
        return nullptr;
    }
}

void IO8042::OnTick(TimerCallback& callback)
{
    (void)callback;

    Stdlib::AutoLock lock(Lock);

    while (!Buf.IsEmpty())
    {
        /* A key's character by its make code, a NUL for a key that has none:
           Escape, Ctrl, the Shifts, Alt, Caps Lock. Backspace is BS, as a
           terminal's is. */
        static char map[0x80] = "\0\0" "1234567890-=\b" "\tqwertyuiop[]\n" "\0" "asdfghjkl;'`"
                                "\0" "\\zxcvbnm,./" "\0" "*\0 \0";
        static char shiftMap[0x80] = "\0\0" "!@#$%^&*()_+\b" "\tQWERTYUIOP{}\n" "\0" "ASDFGHJKL:\"~"
                                     "\0" "|ZXCVBNM<>?" "\0" "*\0 \0";
        u8 code = Buf.Get();

        Trace(KbdLL, "Kbd: code 0x%lX", (ulong)code);

        if (code == CodeExtended)
        {
            /* Extended-key prefix: the next make/break code is an extended
               scancode (arrows, nav keys, right-side modifiers). */
            Extended = 1;
            continue;
        }

        if (Extended)
        {
            Extended = 0;

            /* The right Ctrl is the left one's code behind the prefix */
            if ((code & ~CodeBreak) == CodeCtrl)
            {
                Ctrl = (code & CodeBreak) ? 0 : 1;
                continue;
            }

            /* An arrow is what a terminal sends for it; any other extended
               code is ignored rather than misdecoded into a bogus
               character (or a NUL) via the base scancode map. */
            const char* seq = ArrowSequence(code);
            for (; seq != nullptr && *seq != '\0'; seq++)
                KeyboardInput::GetInstance().Emit(*seq, code);
            continue;
        }

        if (code == CodeLeftShift || code == CodeRightShift) Mod = 1;
        else if (code == (CodeLeftShift | CodeBreak) || code == (CodeRightShift | CodeBreak)) Mod = 0;
        else if (code == CodeCtrl) Ctrl = 1;
        else if (code == (CodeCtrl | CodeBreak)) Ctrl = 0;
        else if (code & CodeBreak) continue;
        else
        {
            BugOn(code >= Stdlib::ArraySize(map));

            char c = Mod ? shiftMap[code] : map[code];
            if (c == '\0')
                continue; /* unmapped key: never deliver a NUL to observers */

            /* Ctrl and a letter is the letter's control byte, as a
               terminal sends it (^R is 0x12); Ctrl and anything else is
               nothing */
            if (Ctrl)
            {
                if (!((c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z')))
                    continue;
                c = static_cast<char>(c & CtrlMask);
            }

            Trace(KbdLL, "Kbd: char %c", c);

            KeyboardInput::GetInstance().Emit(c, code);
        }
    }
}

extern "C" void IO8042Interrupt(Context* ctx)
{
    InterruptStats::Inc(IrqIO8042);
    IO8042::GetInstance().Interrupt(ctx);
}

}
