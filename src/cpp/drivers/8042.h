#pragma once

#include <include/types.h>
#include <kernel/timer.h>
#include <kernel/spin_lock.h>
#include <kernel/interrupt.h>
#include <hal/context.h>
#include <kernel/atomic.h>
#include <kernel/input.h>
#include <lib/ring_buffer.h>

namespace Kernel
{

class IO8042 : public TimerCallback, public InterruptHandler
{
public:
    static IO8042& GetInstance()
    {
        static IO8042 Instance;
        return Instance;
    }

    virtual void OnInterruptRegister(u8 irq, u8 vector) override;
    virtual InterruptHandlerFn GetHandlerFn() override;

    void Interrupt(Context* ctx);

    virtual void OnTick(TimerCallback& callback) override;

    char GetCmd();

private:
    IO8042();
    virtual ~IO8042();
    void ReadData();

    IO8042(const IO8042& other) = delete;
    IO8042(IO8042&& other) = delete;
    IO8042& operator=(const IO8042& other) = delete;
    IO8042& operator=(IO8042&& other) = delete;

    static const ulong DataPort = 0x60;
    static const ulong StatusPort = 0x64;

    /* Bound on the ReadData drain loop (stuck status bit) */
    static const size_t MaxDrainIterations = 4096;

    /* Set-1 scan codes: a key's make code, its break code the same with
       CodeBreak set; behind CodeExtended, the right Ctrl and the arrows */
    static const u8 CodeBreak = 0x80;
    static const u8 CodeExtended = 0xE0;
    static const u8 CodeCtrl = 0x1D;
    static const u8 CodeLeftShift = 0x2A;
    static const u8 CodeRightShift = 0x36;
    static const u8 CodeUp = 0x48;
    static const u8 CodeLeft = 0x4B;
    static const u8 CodeRight = 0x4D;
    static const u8 CodeDown = 0x50;

    /* Ctrl keeps a letter's low five bits: 'r' (0x72) is ^R (0x12) */
    static const u8 CtrlMask = 0x1F;

    /* What a terminal sends for an arrow, which the shell's editor reads:
       nullptr for any other extended key */
    static const char* ArrowSequence(u8 code);

    SpinLock Lock;
    Stdlib::RingBuffer<u8, Const::PageSize> Buf;

    int IntVector;
    u8 Mod;
    u8 Ctrl;
    u8 Extended; /* set after a 0xE0 prefix byte */

    Atomic InterruptCounter;
};

}
