#include "8042.h"
#include "memory_map.h"
#include "trace.h"
#include "asm.h"
#include "pic.h"
#include "vga.h"

namespace Kernel
{

namespace Core
{

IO8042::IO8042()
    : IntNum(-1)
    , Mod(0)
{
}

IO8042::~IO8042()
{
}

void IO8042::RegisterInterrupt(int intNum)
{
    auto& idt = Idt::GetInstance();

    idt.SetDescriptor(intNum, IdtDescriptor::Encode(IO8042InterruptStub));
    IntNum = intNum;
    Shared::Time period;

    period.Secs = 0;
    period.NanoSecs = 10 * 1000 * 1000; // 10ms

    TimerTable::GetInstance().StartTimer(*this, period);
}

void IO8042::UnregisterInterrupt()
{
    if (IntNum >= 0)
    {
        TimerTable::GetInstance().StopTimer(*this);

        auto& idt = Idt::GetInstance();

        idt.SetDescriptor(IntNum, IdtDescriptor::Encode(nullptr));
        IntNum = -1;
    }
}

void IO8042::Interrupt()
{
    auto& io8042 = IO8042::GetInstance();

    io8042.Put(Inb(Port));
    Pic::EOI();
}

bool IO8042::Put(u8 code)
{
    return Buf.Put(code);
}

u8 IO8042::Get()
{
    return Buf.Get();
}

void IO8042::OnTick(TimerCallback& callback)
{
    (void)callback;

    auto& term = VgaTerm::GetInstance();

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

            term.Printf("%c", c);
        }
    }
}

extern "C" void IO8042Interrupt()
{
    IO8042::Interrupt();
}

}
}
