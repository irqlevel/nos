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

    /* Deferred work for this CPU's softirq task: every type `mask` has a bit
       of (1UL << TYPE_*), with one wake and at most one kick of the task for
       all of them -- where a raise each kicked it once each. From any
       context, a hard IRQ handler's included. The work runs on the CPU which
       raised it; a bit past the types is none. */
    void raise_mask(ulong mask);

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
    static const ulong MAX_TYPES = 8;
    static const ulong ALL_TYPES = (1UL << MAX_TYPES) - 1;

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
        /* A type's: the cycle counter at the raise that made it pending, for
           the wait its run's softirq_entry records -- written by that raise
           alone, while softirq_entry is on, and zeroed by the run that takes
           it whether it is on or not, so that none outlives its pending.
           Atomics: a raise on this CPU writes one while its task may read
           it. */
        u64 raised_at[MAX_TYPES];
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

    /* One type of a raise: its stamp, while softirq_entry is on, and its bit
       -- true when this made it pending, false when it already was. */
    bool make_pending(CpuState& state, ulong type);

    /* A type's handler, run once with its records (softirq_entry and
       softirq_exit) where they are on: `raised` the stamp of the raise its
       wait is counted from, 0 for none. */
    void run_handler(CpuState& state, ulong type, u64 raised);

    /* Whether this CPU has a pending type that is not already running
       somewhere else -- the condition its task blocks on. */
    bool has_runnable_work(CpuState& state);

    static const ulong TAG = 'SIrq';
};

}
