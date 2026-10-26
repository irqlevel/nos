#pragma once

#include <include/types.h>
#include <kernel/interrupt.h>

namespace kernel
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
    static GenericTimer& get_instance()
    {
        static GenericTimer instance;
        return instance;
    }

    /* BSP: register the PPI, then start this CPU's timer. */
    bool setup();

    /* Each CPU (incl. APs): enable the timer PPI in its own redistributor
       and start its CNTV. */
    void arm_self();

    /* Fire this CPU's CNTV once, deltaNs from now: its compare value
       written, which also takes the level line down for one in the past
       (Hal::LocalTimerArm). */
    static void arm_in(ulong delta_ns);

    u32 int_id() const { return timer_int_id_; }

    /* Local per-CPU tick: re-arm, run the scheduler (and global timers on
       the BSP), EOI the PPI before Schedule() may switch away. */
    void local_tick(Context* ctx);

    void on_interrupt_register(u8 irq, u8 vector) override;
    InterruptHandlerFn get_handler_fn() override;
    void on_interrupt(Context* ctx) override;

private:
    GenericTimer();
    ~GenericTimer();
    GenericTimer(const GenericTimer& other) = delete;
    GenericTimer(GenericTimer&& other) = delete;
    GenericTimer& operator=(const GenericTimer& other) = delete;
    GenericTimer& operator=(GenericTimer&& other) = delete;

    u32 timer_int_id_;

    static constexpr ulong MAX_ARM_NS = consts::NANO_SECS_IN_SEC;
};

}
