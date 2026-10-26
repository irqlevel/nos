#pragma once

#include <include/types.h>
#include "atomic.h"
#include "task.h"
#include "cpu.h"

namespace kernel
{

class SoftIrq
{
public:
    static SoftIrq& get_instance()
    {
        static SoftIrq instance;
        return instance;
    }

    /* Called during boot: creates a soft IRQ task per running CPU */
    bool init();
    void stop();

    /* Called from hard IRQ handler to schedule deferred work.
       The work runs on the CPU which raised it. */
    void raise(ulong type);

    /* Safety net, called from this CPU's timer tick. Nothing here should be
       needed: every path that makes work available also wakes the task. But
       a wakeup that goes missing would wedge this CPU's deferred work for
       good, on a machine whose only console is a UDP socket. One atomic read
       per tick turns that whole class of bug into at most a tick of latency,
       visible in a profile rather than as a dead box. */
    void tick_kick(ulong cpu);

    /* Whether this CPU already has work of this type queued. A poll that
       wants to cover a lost interrupt must not raise on top of a wakeup that
       has already happened, or it cannot tell the two apart afterwards. */
    bool is_pending(ulong type);

    /* Register a handler for a soft IRQ type */
    void register_handler(ulong type, void (*handler)(void* ctx), void* ctx);

    static const ulong TYPE_NET_RX = 0;
    static const ulong TYPE_BLK_IO = 1;
    static const ulong TYPE_NET_TX = 2;
    static const ulong TYPE_TCP_TIMER = 3;
    static const ulong MAX_TYPES = 8;

private:
    SoftIrq();
    ~SoftIrq();
    SoftIrq(const SoftIrq& other) = delete;
    SoftIrq(SoftIrq&& other) = delete;
    SoftIrq& operator=(const SoftIrq& other) = delete;
    SoftIrq& operator=(SoftIrq&& other) = delete;

    struct Handler
    {
        void (*func)(void* ctx);
        void* ctx;
    };

    struct CpuState
    {
        Atomic pending; /* bitmask of pending soft IRQ types */
        Task* task_ptr;
    };

    Handler handlers_[MAX_TYPES];
    CpuState cpu_states_[MAX_CPUS];

    /* Handlers are not reentrant: a type runs on at most one CPU
       at a time. A CPU which loses the race keeps its pending bit
       set and retries. */
    Atomic running_; /* bitmask of soft IRQ types being handled */

    Atomic ready_; /* set once the per-CPU tasks are started */

    static void task_func(void* ctx);
    void run(CpuState& state);

    /* Whether this CPU has a pending type that is not already running
       somewhere else -- the condition its task blocks on. */
    bool has_runnable_work(CpuState& state);

    static const ulong TAG = 'SIrq';
};

}
