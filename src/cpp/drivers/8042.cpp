#include "8042.h"
#include <arch/x86_64/pic.h>
#include "screen.h"
#include <hal/irqchip.h>

#include <kernel/trace.h>
#include <arch/x86_64/asm.h>
#include <kernel/interrupt.h>
#include <mm/memory_map.h>

namespace kernel
{

IO8042::IO8042()
    : int_vector_(-1)
    , mod_(0)
    , ctrl_(0)
    , extended_(0)
{
    trace(0, "IO8042 0x%p status 0x%lX", this, (ulong)inb(STATUS_PORT));

    screen::printf("IO8042 status 0x%lX\n", (ulong)inb(STATUS_PORT));

    read_data();
}

IO8042::~IO8042()
{
}

void IO8042::on_interrupt_register(u8 irq, u8 vector)
{
    (void)irq;

    int_vector_ = vector;

    stdlib::Time period(10 * consts::NANO_SECS_IN_MS); //10ms

    TimerTable::get_instance().start_timer(*this, period);
}

InterruptHandlerFn IO8042::get_handler_fn()
{
    return io8042_interrupt_stub;
}

void IO8042::read_data()
{
    stdlib::AutoLock lock(lock_);

    /* Bounded drain: a stuck status bit must not hang the boot or the ISR */
    for (size_t i = 0; i < MAX_DRAIN_ITERATIONS; i++)
    {
        if (!(inb(STATUS_PORT) & 0x1))
            break;

        if (!buf_.put(inb(DATA_PORT)))
        {
            trace(0, "Kbd: can't put new code");
        }
    }
}

void IO8042::interrupt(Context* ctx)
{
    (void)ctx;

    interrupt_counter_.inc();
    read_data();
    hal::irq_eoi(int_vector_);
}


const char* IO8042::arrow_sequence(u8 code)
{
    switch (code)
    {
    case CODE_UP:
        return "\x1b[A";
    case CODE_DOWN:
        return "\x1b[B";
    case CODE_RIGHT:
        return "\x1b[C";
    case CODE_LEFT:
        return "\x1b[D";
    default:
        return nullptr;
    }
}

void IO8042::on_tick(TimerCallback& callback)
{
    (void)callback;

    stdlib::AutoLock lock(lock_);

    while (!buf_.is_empty())
    {
        /* A key's character by its make code, a NUL for a key that has none:
           Escape, Ctrl, the Shifts, Alt, Caps Lock. Backspace is BS, as a
           terminal's is. */
        static char map[0x80] = "\0\0" "1234567890-=\b" "\tqwertyuiop[]\n" "\0" "asdfghjkl;'`"
                                "\0" "\\zxcvbnm,./" "\0" "*\0 \0";
        static char shift_map[0x80] = "\0\0" "!@#$%^&*()_+\b" "\tQWERTYUIOP{}\n" "\0" "ASDFGHJKL:\"~"
                                     "\0" "|ZXCVBNM<>?" "\0" "*\0 \0";
        u8 code = buf_.get();

        trace(KBD_LL, "Kbd: code 0x%lX", (ulong)code);

        if (code == CODE_EXTENDED)
        {
            /* Extended-key prefix: the next make/break code is an extended
               scancode (arrows, nav keys, right-side modifiers). */
            extended_ = 1;
            continue;
        }

        if (extended_)
        {
            extended_ = 0;

            /* The right Ctrl is the left one's code behind the prefix */
            if ((code & ~CODE_BREAK) == CODE_CTRL)
            {
                ctrl_ = (code & CODE_BREAK) ? 0 : 1;
                continue;
            }

            /* An arrow is what a terminal sends for it; any other extended
               code is ignored rather than misdecoded into a bogus
               character (or a NUL) via the base scancode map. */
            const char* seq = arrow_sequence(code);
            for (; seq != nullptr && *seq != '\0'; seq++)
                KeyboardInput::get_instance().emit(*seq, code);
            continue;
        }

        if (code == CODE_LEFT_SHIFT || code == CODE_RIGHT_SHIFT) mod_ = 1;
        else if (code == (CODE_LEFT_SHIFT | CODE_BREAK) || code == (CODE_RIGHT_SHIFT | CODE_BREAK)) mod_ = 0;
        else if (code == CODE_CTRL) ctrl_ = 1;
        else if (code == (CODE_CTRL | CODE_BREAK)) ctrl_ = 0;
        else if (code & CODE_BREAK) continue;
        else
        {
            BUG_ON(code >= stdlib::array_size(map));

            char c = mod_ ? shift_map[code] : map[code];
            if (c == '\0')
                continue; /* unmapped key: never deliver a NUL to observers */

            /* Ctrl and a letter is the letter's control byte, as a
               terminal sends it (^R is 0x12); Ctrl and anything else is
               nothing */
            if (ctrl_)
            {
                if (!((c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z')))
                    continue;
                c = static_cast<char>(c & CTRL_MASK);
            }

            trace(KBD_LL, "Kbd: char %c", c);

            KeyboardInput::get_instance().emit(c, code);
        }
    }
}

extern "C" void io8042_interrupt(Context* ctx)
{
    InterruptStats::inc(IrqIO8042);
    IO8042::get_instance().interrupt(ctx);
}

}
