#pragma once

#include <include/types.h>
#include <kernel/interrupt.h>

namespace Kernel
{

/* ARM generic (virtual) timer scheduler tick. Each CPU runs its own timer:
   the PPI (INTID from the DTB, 27 on QEMU virt) is banked per-CPU, so every
   CPU arms its own CNTV and services the tick locally (no BSP broadcast).
   The tick is dispatched specially by ArmIrqEntry -> LocalTick, which EOIs
   before Schedule() may switch away (mirroring the IPI path). One-shot, by
   its compare value: armed for each tick and for each deadline a task on
   the CPU sleeps to (Cpu::LocalTimer*, Hal::LocalTimerArm). */
class GenericTimer final : public InterruptHandler
{
public:
    static GenericTimer& GetInstance()
    {
        static GenericTimer Instance;
        return Instance;
    }

    /* BSP: register the PPI, then start this CPU's timer. */
    bool Setup();

    /* Each CPU (incl. APs): enable the timer PPI in its own redistributor
       and start its CNTV. */
    void ArmSelf();

    /* Fire this CPU's CNTV once, deltaNs from now: its compare value
       written, which also takes the level line down for one in the past
       (Hal::LocalTimerArm). */
    static void ArmIn(ulong deltaNs);

    u32 IntId() const { return TimerIntId; }

    /* Local per-CPU tick: re-arm, run the scheduler (and global timers on
       the BSP), EOI the PPI before Schedule() may switch away. */
    void LocalTick(Context* ctx);

    void OnInterruptRegister(u8 irq, u8 vector) override;
    InterruptHandlerFn GetHandlerFn() override;
    void OnInterrupt(Context* ctx) override;

private:
    GenericTimer();
    ~GenericTimer();
    GenericTimer(const GenericTimer& other) = delete;
    GenericTimer(GenericTimer&& other) = delete;
    GenericTimer& operator=(const GenericTimer& other) = delete;
    GenericTimer& operator=(GenericTimer&& other) = delete;

    u32 TimerIntId;

    static constexpr ulong MaxArmNs = Const::NanoSecsInSec;
};

}
