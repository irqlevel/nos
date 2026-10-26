#pragma once

#include "stdlib.h"
#include "spin_lock.h"
#include "raw_spin_lock.h"
#include "wait_group.h"
#include "task.h"
#include "sched.h"
#include <hal/cpu.h>
#include <hal/irqchip.h>
#include <hal/context.h>

namespace kernel
{

/* Upper bound on the CPUs the kernel tracks.  64 is a ceiling rather than a
   tuning knob: CPU sets travel as a ulong bitmask (GetRunningCpus,
   Task::CpuAffinity, SoftIrq), so index 63 is the last one that fits.

   On x86 the index *is* the LAPIC ID, not a dense counter, so this bound has
   to clear the largest APIC ID the firmware reports rather than the CPU
   count: a 14-core Raptor Lake numbers its 20 threads 0,1,8,9,...,62, and at
   MaxCpus = 8 everything above the first two threads was dropped.  Anything
   sized by this constant is per-CPU static storage (kernel/main.cpp Stack[],
   gdt.h DfStack[], arm64 ApBootStack[]), so raising it further costs BSS in
   proportion. */
const ulong MAX_CPUS = 64;

/* The tick: every CPU's, 100 times a second on both architectures. The
   round robin, the watchdog, the profiler and the software timers run on
   it; a task asleep until a deadline does not wait for it (Cpu::LocalTimer*). */
const ulong TICK_HZ = 100;

struct IPITask
{
    using Func = void (*)(void* ctx, Context* ipi_ctx);

    IPITask();
    IPITask(Func func, void* ctx);

    Func function;
    void* ctx;
    WaitGroup completion;
    stdlib::ListEntry list_entry;
};

class Cpu final
{
public:
    Cpu();
    ~Cpu();

    void init(ulong index);

    void set_running();
    void set_exiting();

    ulong get_index();

    void idle();

    ulong get_state();

    /* True once Park() has stopped this CPU for good -- the last thing it
       does, from a stack of its own -- after which the CPU that asked it to
       exit may free everything it ran on: the task it interrupted, that
       task's stack, at last the heap. Written once by the parking CPU and
       read without the lock: by the halt waiting for it
       (CpuTable::ExitAllExceptSelf), by whatever would send it work, and by
       whichever CPU panics. */
    bool is_parked();

    void ipi(Context* ctx);
    void timer_tick(Context* ctx);

    /* This CPU's local timer, one-shot: armed for the earlier of its next
       tick, on a TickHz grid, and the soonest deadline of a task asleep on
       it -- so a Sleep() or an Event::WaitFor() ends at its deadline, and
       not at the next tick after it. Each is called on this CPU only, with
       interrupts off.

       LocalTimerStart: the hardware set up one-shot by the arch, the grid
       begins and the first tick is armed. LocalTimerFired: from the
       timer's interrupt, whether the tick is due -- the grid moved on if so
       -- with the timer armed again for the next one; the soonest deadline,
       if it is sooner, is the scheduler's to bring forward
       (LocalTimerTakeWalk, TaskQueue::Schedule). LocalTimerWake: the timer
       brought forward to a deadline sooner than the one it has; nothing for
       0, for a later one, or where the timer is not one-shot.
       LocalTimerTick: the next tick armed and nothing else, for a CPU whose
       queue may not be looked at -- a panic's. */
    void local_timer_start();
    bool local_timer_one_shot() const { return timer_one_shot_; }
    bool local_timer_fired(ulong now);
    bool local_timer_take_walk();
    void local_timer_wake(ulong deadline);
    void local_timer_tick();

    static const ulong STATE_INITED = 0x1;
    static const ulong STATE_RUNNING = 0x2;
    static const ulong STATE_EXITING = 0x4;

    bool run(Task::Func func, void *ctx);

    /* The task Run() made for this CPU: the BSP's runs BpStartup and then
       the shutdown watch, an AP's its idle loop */
    Task* get_idle_task() { return idle_task_ptr_; }

    void send_ipi_self();

    void queue_ipi_task(IPITask& task);
    void queue_ipi_task_async(IPITask& task);

    TaskQueue& get_task_queue();

    void reset();

private:
    Cpu(const Cpu& other) = delete;
    Cpu(Cpu&& other) = delete;
    Cpu& operator=(const Cpu& other) = delete;
    Cpu& operator=(Cpu&& other) = delete;

    void on_panic();
    void __attribute__((noreturn)) park();
    static void __attribute__((noreturn)) park_on_own_stack(void* ctx);
    void process_ipi_tasks(Context* ctx);
    void drain_and_close_ipi_tasks(Context* ctx);

    ulong index_;
    ulong state_;
    SpinLock lock_;
    Task* idle_task_ptr_;
    TaskQueue task_queue_;
    Atomic ipi_conter_;

    RawSpinLock ipi_task_lock_;
    stdlib::ListEntry ipi_task_list_;
    bool ipi_tasks_closed_; /* set on exit: queuers self-complete instead of waiting */

    /* Set by Park(), last; see IsParked */
    Atomic parked_;

    void local_timer_arm(ulong when, ulong now);

    /* The local timer (LocalTimer*): one-shot here, the next tick's due
       time, when the timer is armed to fire (~0 when it is not), and that
       it fired since the scheduler last looked for the soonest deadline.
       This CPU's alone, with interrupts off. */
    bool timer_one_shot_;
    ulong tick_due_;
    ulong timer_armed_at_;
    bool timer_walk_;

    /* Ticks are a period apart; an interrupt this close before the next
       is that tick, rather than one more interrupt a moment later. */
    static constexpr ulong TICK_PERIOD_NS = consts::NANO_SECS_IN_SEC / TICK_HZ;
    static constexpr ulong TICK_SLACK_NS = 20 * consts::NANO_SECS_IN_USEC;
    /* The nearest the timer is armed: one already due fires at once. */
    static constexpr ulong TIMER_MIN_NS = consts::NANO_SECS_IN_USEC;

    static const ulong TAG = 'Cpu ';
};

class CpuTable final
{
public:
    static CpuTable& get_instance()
    {
        static CpuTable instance;
        return instance;
    }

    bool insert_cpu(ulong index);

    Cpu& get_cpu(ulong index);

    bool start_all();

    ulong get_bsp_index();
    bool set_bsp_index(ulong index);

    /* Lock-free BSP index for IRQ context (e.g. the tick IPI) */
    ulong get_bsp_index_no_lock();

    ulong get_current_cpu_id();

    Cpu& get_current_cpu();

    static const u8 IPI_VECTOR = hal::IPI_VECTOR;

    /* The local APIC timer's own vector, below the IPI's. */
    static const u8 TIMER_VECTOR = 0xFD;

    /* True once every CPU is ticking off its own local timer; until then the
       tick arrives as a broadcast IPI from whichever CPU owns the HPET. */
    bool has_per_cpu_timer();
    void set_per_cpu_timer();

    void send_ipi(ulong index);

    ulong get_running_cpus();

    /* Mirror of the above, readable without taking Lock. The watchdog reads
       it from every CPU's tick, and taking the table lock there would be both
       the contention the reader exists to remove and a spinlock acquired
       from an interrupt with interrupts off -- the shape that wedges
       machines. Published by NoteCpuRunning as each CPU comes up, and
       refreshed by every locked call above. Nothing clears StateRunning
       today, so nothing takes a bit back out. */
    ulong get_running_cpus_no_lock();

    void note_cpu_running(ulong index);

    void exit_all_except_self();

    void send_ipi_all_exclude(ulong exclude_index);

    void send_ipi_all();

    void invalidate_tlb_all();
    void invalidate_tlb_address(ulong virt_addr);
    void invalidate_tlb_range(ulong virt_addr, ulong count);

    void reset();

private:
    CpuTable();
    ~CpuTable();
    CpuTable(const CpuTable& other) = delete;
    CpuTable(CpuTable&& other) = delete;
    CpuTable& operator=(const CpuTable& other) = delete;
    CpuTable& operator=(CpuTable&& other) = delete;

    ulong get_bsp_index_lock_held();
    ulong get_remote_cpu_mask();
    void send_tlb_ipi(ulong cpu_mask, IPITask tasks[MAX_CPUS]);

    SpinLock lock_;
    Cpu cpu_array_[MAX_CPUS];

    ulong bsp_index_;
    /* Mirror of BspIndex readable without taking Lock */
    Atomic bsp_index_cached_;

    /* Mirror of the running-CPU set, read on the tick path */
    Atomic running_mask_cached_;


    /* Read on the tick path from every CPU; written once, at boot. */
    volatile bool per_cpu_timer_;
};

static inline Cpu& get_cpu()
{
    return CpuTable::get_instance().get_current_cpu();
}

}