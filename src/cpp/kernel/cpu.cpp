#include "cpu.h"
#include "interrupt.h"
#include "panic.h"
#include "trace.h"
#include "watchdog.h"
#include "softirq.h"
#include "profiler.h"
#include "timer.h"

#include <hal/irqchip.h>
#include <hal/mmu.h>
#include <hal/timer.h>

#include <kernel/time.h>
#include <mm/new.h>
#include <mm/page_table.h>

/* The network layer is Rust (src/rust/net): a receive pass offered from the
   tick, which it takes only with rxpoll=on. */
extern "C" void rust_net_poll_rx();

namespace kernel
{

IPITask::IPITask()
    : function(nullptr)
    , ctx(nullptr)
    , completion(1)
{
}

IPITask::IPITask(Func func, void* ctx)
    : function(func)
    , ctx(ctx)
    , completion(1)
{
}

Cpu::Cpu()
    : index_(0)
    , state_(0)
    , idle_task_ptr_(nullptr)
    , ipi_tasks_closed_(false)
    , timer_one_shot_(false)
    , tick_due_(0)
    , timer_armed_at_(~0UL)
    , timer_walk_(false)
{
    ipi_task_list_.init();
}

ulong Cpu::get_index()
{
    return index_;
}

void Cpu::idle()
{
    if (BUG_ON(!(state_ & STATE_RUNNING)))
        return;

    if (BUG_ON(!hal::is_interrupt_enabled()))
        return;

    /* Free tasks that exited on this CPU (deferred out of the IRQs-off switch
       path); safe here because interrupts are enabled. */
    get_task_queue().reap_exited();

    /* The last resort from here until it wakes: an interrupt that finds
       something else to run switches to it and leaves this task in its
       place, rather than in the turns (Task::set_idling). Not before the
       reap, whose frees may wait for other CPUs. */
    Task* self = Task::try_get_current_task();
    if (self != nullptr)
        self->set_idling(true);
    cpu_halt();
    if (self != nullptr)
        self->set_idling(false);
}

ulong Cpu::get_state()
{
    stdlib::AutoLock lock(lock_);
    return state_;
}

bool Cpu::is_parked()
{
    return (parked_.get() != 0) ? true : false;
}

void Cpu::set_running()
{
    {
        stdlib::AutoLock lock(lock_);
        if (BUG_ON(state_ & STATE_RUNNING))
            return;

        state_ |= STATE_RUNNING;
    }

    /* Published outside this CPU's own lock and without the table's: the
       watchdog's tick path reads this mirror on every CPU. */
    CpuTable::get_instance().note_cpu_running(index_);
}

void Cpu::set_exiting()
{
    stdlib::AutoLock lock(lock_);
    state_ |= STATE_EXITING;
}

void Cpu::init(ulong index)
{
    stdlib::AutoLock lock(lock_);
    if (BUG_ON(state_ & STATE_INITED))
        return;

    index_ = index;
    state_ |= STATE_INITED;

    trace(0, "Cpu 0x%p %lu inited", this, index_);
}

void Cpu::reset()
{
    stdlib::AutoLock lock(lock_);

    BUG_ON(state_ == STATE_RUNNING);
    task_queue_.clear();

    if (idle_task_ptr_ != nullptr)
    {
        idle_task_ptr_->put();
        idle_task_ptr_ = nullptr;
    }
}

Cpu::~Cpu()
{
    reset();
}

CpuTable::CpuTable()
    : bsp_index_(0)
{
}

CpuTable::~CpuTable()
{
    reset();
}

bool CpuTable::insert_cpu(ulong index)
{
    stdlib::AutoLock lock(lock_);
    if (index >= stdlib::array_size(cpu_array_))
        return false;

    auto& cpu = cpu_array_[index];
    if (cpu.get_state() & Cpu::STATE_INITED)
        return false;

    cpu.init(index);
    return true;
}

Cpu& CpuTable::get_cpu(ulong index)
{
    BUG_ON(index >= stdlib::array_size(cpu_array_));
    Cpu& cpu = cpu_array_[index];
    return cpu;
}

ulong CpuTable::get_bsp_index()
{
    stdlib::AutoLock lock(lock_);
    return get_bsp_index_lock_held();
}

ulong CpuTable::get_bsp_index_lock_held()
{
    return bsp_index_;
}

ulong CpuTable::get_bsp_index_no_lock()
{
    return (ulong)bsp_index_cached_.get();
}

bool CpuTable::set_bsp_index(ulong index)
{
    stdlib::AutoLock lock(lock_);

    if (BUG_ON(index >= stdlib::array_size(cpu_array_)))
        return false;

    auto& cpu = cpu_array_[index];
    if (BUG_ON(!(cpu.get_state() & Cpu::STATE_INITED)))
        return false;

    cpu.set_running();
    bsp_index_ = index;
    bsp_index_cached_.set((long)index);
    return true;
}

ulong CpuTable::get_current_cpu_id()
{
    return hal::get_current_cpu_hw_id();
}

Cpu& CpuTable::get_current_cpu()
{
    return get_cpu(get_current_cpu_id());
}

void CpuTable::exit_all_except_self()
{
    auto& self = get_current_cpu();

    ulong cpu_mask = get_running_cpus();
    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        if ((cpu_mask & (1UL << i)) && (i != self.get_index()))
        {
            auto& cpu = get_cpu(i);
            cpu.set_exiting();

            while (!cpu.is_parked())
            {
                cpu.send_ipi_self();
                cpu_relax();
            }
        }
    }
}

void CpuTable::send_ipi_all_exclude(ulong exclude_index)
{
    ulong cpu_mask = get_running_cpus();
    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        if ((cpu_mask & (1UL << i)) && (i != exclude_index))
        {
            auto& cpu = get_cpu(i);

            cpu.send_ipi_self();
        }
    }
}

void CpuTable::send_ipi_all()
{
    ulong cpu_mask = get_running_cpus();
    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        if ((cpu_mask & (1UL << i)))
        {
            auto& cpu = get_cpu(i);

            cpu.send_ipi_self();
        }
    }
}

void Cpu::on_panic()
{
    interrupt_disable();
    for (;;)
    {
        cpu_relax();
    }
}

/* What a parked CPU stands on (Cpu::park): a stack of its own, static, which
   nothing frees -- so that whatever a parked CPU still runs, it touches
   nothing the halt takes down. That is its last two calls and the halt; on
   x86 also an NMI or a machine check, which end HLT with interrupts off,
   push their frames onto the stack the CPU is on, and may panic there. A
   panic is what the size is for: one made to run on this stack (stack_probe,
   on a build for the purpose) went 1800 bytes deep on arm64 and 1880 on
   x86, and 5176 on arm64 with the netconsole sending its report. */
static const ulong PARK_STACK_SIZE = 4 * consts::PAGE_SIZE;
static char park_stack[MAX_CPUS][PARK_STACK_SIZE] __attribute__((aligned(16)));

/* Where an exiting CPU ends, for good. Once it is parked (is_parked) the CPU
   that asked goes on to free the tasks, their stacks and at last the heap --
   the stack this one was interrupted on among them -- so from that moment
   this CPU must touch none of it, for as long as the machine runs.

   It used to say so on that stack and stay there. One halt is not enough to
   stay put: arm64's WFI may complete at any moment -- a pending interrupt
   ends it even while masked, and under HVF every AP was out of it within
   milliseconds -- and x86's HLT ends at an NMI, whose frame goes onto the
   stack the CPU is on. The halt was a loop around cpu_halt(), a call, whose frame
   record went onto that stack at every wakeup: the data abort a parked AP
   printed now and then on arm64, in the middle of the boot CPU's static
   destructors. And the word that it had exited was written under this CPU's
   lock, whose release went on, past the moment the other CPU could see the
   word, to write the preemption count of the task it had interrupted.

   So it leaves that stack first, for its own, says it is parked from there
   -- an atomic store and not this CPU's lock, which off a task's stack would
   look for the current task and panic -- and halts in a loop that touches no
   memory at all. */
void Cpu::park()
{
    interrupt_disable();

    hal::run_on_stack((ulong)&park_stack[index_][PARK_STACK_SIZE], park_on_own_stack, this);
}

void Cpu::park_on_own_stack(void* ctx)
{
    auto cpu = static_cast<Cpu*>(ctx);

    cpu->parked_.set(1);

    hal::halt_forever();
}

static void tlb_flush_func(void* ctx, Context* ipi_ctx)
{
    (void)ctx;
    (void)ipi_ctx;
    mm::PageTable::invalidate_local_tlb();
}

struct TlbRangeCtx
{
    ulong virt_addr;
    ulong count;
};

static void tlb_flush_range_func(void* ctx, Context* ipi_ctx)
{
    (void)ipi_ctx;
    auto* rc = (TlbRangeCtx*)ctx;
    mm::PageTable::invalidate_local_tlb_range(rc->virt_addr, rc->count);
}

/* Build a running-CPU mask excluding the local CPU and exited CPUs. */
ulong CpuTable::get_remote_cpu_mask()
{
    ulong local_id = get_current_cpu_id();
    ulong cpu_mask = get_running_cpus() & ~(1UL << local_id);

    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        if ((cpu_mask & (1UL << i)) && cpu_array_[i].is_parked())
            cpu_mask &= ~(1UL << i);
    }

    return cpu_mask;
}

void CpuTable::send_tlb_ipi(ulong cpu_mask, IPITask tasks[MAX_CPUS])
{
    /* A shootdown that never completes means some CPU cannot take the IPI,
       and in this kernel that has exactly one cause: it is spinning on a
       lock with interrupts off -- usually a lock this CPU is holding while
       it frees the page it is shooting down. Waiting for it forever produces
       a machine that stops dead with no fault to report and, if the lock is
       in the network path, no way to report it either. Ten seconds is far
       past any legitimate round of IPIs; name the CPUs that never answered
       and panic, which at least prints and can be read afterwards. */
    static const ulong ACK_TIMEOUT_NS = 10 * consts::NANO_SECS_IN_SEC;

    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        if (cpu_mask & (1UL << i))
            cpu_array_[i].queue_ipi_task_async(tasks[i]);
    }

    stdlib::Time deadline = get_boot_time() + stdlib::Time(ACK_TIMEOUT_NS);
    ulong silent = 0;

    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        if (!(cpu_mask & (1UL << i)))
        {
            tasks[i].completion.done();
            continue;
        }

        while (tasks[i].completion.get_counter() != 0)
        {
            if (get_boot_time() >= deadline)
            {
                silent |= (1UL << i);
                break;
            }

            /* yield_to_runnable, not Schedule: the ack comes from the remote
               CPU's IPI handler, which has no reason to interrupt this one.
               Handing this CPU to the idle task therefore halts it until its
               own next tick, and a shootdown that the other CPUs answered in
               microseconds took a whole one -- on the path every mm::free
               with a mapping to drop goes down (see WaitGroup::wait). */
            yield_to_runnable();
        }
    }

    if (silent != 0)
        PANIC("TLB shootdown: cpus 0x%lX never acknowledged (asked 0x%lX)",
            silent, cpu_mask);
}

void CpuTable::invalidate_tlb_all()
{
    mm::PageTable::invalidate_local_tlb();

    if (!hal::tlb_shootdown_needs_ipi())
        return; /* the local TLBI already broadcast to all CPUs */

    ulong cpu_mask = get_remote_cpu_mask();
    if (cpu_mask == 0)
        return;

    IPITask tasks[MAX_CPUS];

    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        if (cpu_mask & (1UL << i))
            tasks[i].function = tlb_flush_func;
    }

    send_tlb_ipi(cpu_mask, tasks);
}

void CpuTable::invalidate_tlb_address(ulong virt_addr)
{
    mm::PageTable::invalidate_local_tlb_address(virt_addr);

    if (!hal::tlb_shootdown_needs_ipi())
        return;

    ulong cpu_mask = get_remote_cpu_mask();
    if (cpu_mask == 0)
        return;

    TlbRangeCtx rc;
    rc.virt_addr = virt_addr;
    rc.count = 1;

    IPITask tasks[MAX_CPUS];

    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        if (cpu_mask & (1UL << i))
        {
            tasks[i].function = tlb_flush_range_func;
            tasks[i].ctx = &rc;
        }
    }

    send_tlb_ipi(cpu_mask, tasks);
}

void CpuTable::invalidate_tlb_range(ulong virt_addr, ulong count)
{
    mm::PageTable::invalidate_local_tlb_range(virt_addr, count);

    if (!hal::tlb_shootdown_needs_ipi())
        return;

    ulong cpu_mask = get_remote_cpu_mask();
    if (cpu_mask == 0)
        return;

    TlbRangeCtx rc;
    rc.virt_addr = virt_addr;
    rc.count = count;

    IPITask tasks[MAX_CPUS];

    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        if (cpu_mask & (1UL << i))
        {
            tasks[i].function = tlb_flush_range_func;
            tasks[i].ctx = &rc;
        }
    }

    send_tlb_ipi(cpu_mask, tasks);
}

void Cpu::ipi(Context* ctx)
{
    ipi_conter_.inc();

    process_ipi_tasks(ctx);

    if (Panicker::get_instance().is_active())
    {
        on_panic();
        return;
    }

    bool exit;
    {
        stdlib::AutoLock lock(lock_);
        exit = (state_ & STATE_EXITING) ? true : false;
    }

    if (exit)
    {
        trace(0, "Cpu %lu exited, state 0x%lX, IPI count %lu",
            index_, state_, ipi_conter_.get());

        /* Complete any IPI tasks queued during the exit transition and refuse
           future ones, so a CPU that queued work to us never waits forever. */
        drain_and_close_ipi_tasks(ctx);

        park();
    }

    /* The periodic work moved to this CPU's own timer (Cpu::timer_tick).
       While that timer is running the IPI carries only queued work; on a
       machine where it could not be calibrated, the tick still arrives here
       as a broadcast from whichever CPU owns the HPET. */
    if (!CpuTable::get_instance().has_per_cpu_timer())
    {
        Watchdog::get_instance().check();

        /* Index is the LAPIC APIC ID; the BSP's is not necessarily 0 */
        if (index_ == CpuTable::get_instance().get_bsp_index_no_lock())
            TimerTable::get_instance().process_timers();
    }

    hal::irq_eoi(CpuTable::IPI_VECTOR);

    /* Not schedule(): a task this lands on while it can still run keeps the
       CPU -- see Preempt */
    preempt();
}

/* The local timer, one-shot. It used to tick periodically and nothing else,
   and a task asleep until a deadline -- sleep(), Event::wait_for() -- ran at
   the first tick after it: a halted guest's vCPU waits for its guest's next
   timer edge that way, and a guest's `usleep 1000` took 10 ms on the EX44.
   Now the timer fires at the earlier of the next tick and the soonest such
   deadline on this CPU, and the scheduler's walk that follows runs the
   sleeper. The tick keeps its grid and its work; a deadline's interrupt only
   reschedules. */
void Cpu::local_timer_start()
{
    ulong flags = hal::irq_save();
    ulong now = get_boot_time().get_value();

    timer_one_shot_ = true;
    tick_due_ = now + TICK_PERIOD_NS;
    local_timer_arm(tick_due_, now);

    hal::irq_restore(flags);
}

void Cpu::local_timer_arm(ulong when, ulong now)
{
    ulong delta = (when > now) ? when - now : 0;

    /* Never further than a tick and its slack, whatever `when` says: a
       clock that stepped back would otherwise leave the CPU without a tick. */
    delta = stdlib::max(delta, TIMER_MIN_NS);
    delta = stdlib::min(delta, TICK_PERIOD_NS + TICK_SLACK_NS);

    timer_armed_at_ = now + delta;
    hal::local_timer_arm(delta);
}

bool Cpu::local_timer_fired(ulong now)
{
    if (!timer_one_shot_)
        return true;

    bool tick = (now + TICK_SLACK_NS >= tick_due_);
    if (tick)
    {
        /* On the grid; a tick missed -- interrupts off a long while -- is
           not made up, the next one a period from now */
        tick_due_ += TICK_PERIOD_NS;
        if (tick_due_ <= now)
            tick_due_ = now + TICK_PERIOD_NS;
    }

    /* The tick, for now: a sleeper's deadline sooner than it is brought
       forward by the walk the reschedule after this makes, under the queue
       lock it takes anyway (TaskQueue::schedule) -- not from here, with
       interrupts off and whatever this CPU was doing interrupted. */
    local_timer_arm(tick_due_, now);
    timer_walk_ = true;

    return tick;
}

bool Cpu::local_timer_take_walk()
{
    bool walk = timer_walk_;
    timer_walk_ = false;
    return walk;
}

void Cpu::local_timer_wake(ulong deadline)
{
    if (!timer_one_shot_ || deadline == 0 || deadline >= timer_armed_at_)
        return;

    local_timer_arm(deadline, get_boot_time().get_value());
}

void Cpu::local_timer_tick()
{
    if (!timer_one_shot_)
        return;

    ulong now = get_boot_time().get_value();
    if (tick_due_ <= now)
        tick_due_ = now + TICK_PERIOD_NS;
    local_timer_arm(tick_due_, now);
}

/* This CPU's own tick. One interrupt per CPU delivered by its own local
   APIC, instead of one interrupt on one CPU followed by an IPI to every
   other -- which cost a fan of ICR writes per tick and, worse, made the
   machine's entire sense of time depend on one CPU still answering
   interrupts. A CPU that wedges now stops only its own tick, and the others
   go on ticking and notice. One-shot, and also for a sleeper's deadline
   (local_timer_start): such an interrupt only reschedules. */
void Cpu::timer_tick(Context* ctx)
{
    if (Panicker::get_instance().is_active())
    {
        /* Ticking on, as the periodic timer did, without the walk: the
           queue's lock may be the panicking CPU's */
        local_timer_tick();
        hal::irq_eoi(CpuTable::TIMER_VECTOR);
        return;
    }

    if (!local_timer_fired(get_boot_time().get_value()))
    {
        InterruptStats::inc(IrqTimerWake);
        hal::irq_eoi(CpuTable::TIMER_VECTOR);
        preempt();
        return;
    }

    InterruptStats::inc(IrqLapicTimer);

    Watchdog::get_instance().check();

    Profiler::get_instance().tick(ctx);

    /* Software timers stay on one CPU: they are a handful of periodic
       callbacks, not a per-CPU wheel, and running them everywhere would
       just fire each one N times. */
    if (index_ == CpuTable::get_instance().get_bsp_index_no_lock())
    {
        TimerTable::get_instance().process_timers();

        /* And offer the receive path a look, whether or not a NIC asked.
           The network layer takes it only with rxpoll=on -- the switch is
           in its poll, DeviceTable::poll_rx, not here. */
        rust_net_poll_rx();
    }

    /* Repairs a wakeup that went missing, and nothing else -- see
       SoftIrq::tick_kick. Before the reschedule below, so the kick has an
       effect on this very tick. */
    SoftIrq::get_instance().tick_kick(index_);

    hal::irq_eoi(CpuTable::TIMER_VECTOR);

    /* Round-robin among the runnable, and never the idle task over a task
       that can still run -- see Preempt */
    preempt();
}

void Cpu::queue_ipi_task_async(IPITask& task)
{
    ulong flags = ipi_task_lock_.lock_irq_save();
    bool closed = ipi_tasks_closed_;
    if (!closed)
        ipi_task_list_.insert_tail(&task.list_entry);
    ipi_task_lock_.unlock_irq_restore(flags);

    if (closed)
    {
        /* Target CPU has stopped processing IPI tasks (exiting/exited); the
           task will never run there, so complete it now to unblock the waiter
           instead of leaving it queued forever. */
        task.completion.done();
        return;
    }

    send_ipi_self();
}

void Cpu::queue_ipi_task(IPITask& task)
{
    queue_ipi_task_async(task);
    task.completion.wait();
}

void Cpu::process_ipi_tasks(Context* ctx)
{
    stdlib::ListEntry local_list;
    local_list.init();

    ulong flags = ipi_task_lock_.lock_irq_save();
    local_list.move_tail_list(&ipi_task_list_);
    ipi_task_lock_.unlock_irq_restore(flags);

    while (!local_list.is_empty())
    {
        auto* entry = local_list.remove_head();
        IPITask* task = CONTAINING_RECORD(entry, IPITask, list_entry);
        task->function(task->ctx, ctx);
        task->completion.done();
    }
}

void Cpu::drain_and_close_ipi_tasks(Context* ctx)
{
    stdlib::ListEntry local_list;
    local_list.init();

    /* Close the queue and take everything still on it under the same lock, so
       a concurrent queue_ipi_task_async either lands here (and is completed below)
       or observes the closed flag and self-completes -- never both, never
       neither. */
    ulong flags = ipi_task_lock_.lock_irq_save();
    ipi_tasks_closed_ = true;
    local_list.move_tail_list(&ipi_task_list_);
    ipi_task_lock_.unlock_irq_restore(flags);

    while (!local_list.is_empty())
    {
        auto* entry = local_list.remove_head();
        IPITask* task = CONTAINING_RECORD(entry, IPITask, list_entry);
        task->function(task->ctx, ctx);
        task->completion.done();
    }
}

TaskQueue& Cpu::get_task_queue()
{
    return task_queue_;
}

extern "C" void ip_interrupt(Context* ctx)
{
    InterruptStats::inc(IrqIPI);
    auto& cpu = CpuTable::get_instance().get_current_cpu();
    cpu.ipi(ctx);
}

/* A posted interrupt's notification that found its vCPU out of its guest
   (hal::POSTED_VECTOR): the vCPU folds what was posted for it into its APIC
   before it next enters, so there is nothing to do but end the interrupt. */
extern "C" void posted_interrupt(Context* ctx)
{
    (void)ctx;
    InterruptStats::inc(IrqPosted);
    hal::irq_eoi(hal::POSTED_VECTOR);
}

extern "C" void lapic_timer_interrupt(Context* ctx)
{
    auto& cpu = CpuTable::get_instance().get_current_cpu();
    cpu.timer_tick(ctx);
}

void Cpu::send_ipi_self()
{
    stdlib::AutoLock lock(lock_);

    if (BUG_ON(!(state_ & Cpu::STATE_RUNNING)))
        return;

    if (is_parked())
        return;

    hal::send_ipi(index_, CpuTable::IPI_VECTOR);
}

void CpuTable::send_ipi(ulong index)
{
    stdlib::AutoLock lock(lock_);

    if (BUG_ON(index >= stdlib::array_size(cpu_array_)))
        return;

    auto& cpu = cpu_array_[index];
    cpu.send_ipi_self();
}

bool CpuTable::has_per_cpu_timer()
{
    return per_cpu_timer_;
}

void CpuTable::set_per_cpu_timer()
{
    per_cpu_timer_ = true;
}

ulong CpuTable::get_running_cpus_no_lock()
{
    return (ulong)running_mask_cached_.get();
}

void CpuTable::note_cpu_running(ulong index)
{
    if (index >= MAX_CPUS)
        return;

    ulong bit = 1UL << index;
    for (;;)
    {
        ulong old = (ulong)running_mask_cached_.get();
        if ((old & bit) != 0)
            return;

        if ((ulong)running_mask_cached_.cmpxchg((long)(old | bit), (long)old) == old)
            return;
    }
}

ulong CpuTable::get_running_cpus()
{
    stdlib::AutoLock lock(lock_);

    ulong result = 0;
    for (ulong i = 0; i < stdlib::array_size(cpu_array_); i++)
    {
        auto& cpu = cpu_array_[i];
        if (cpu.get_state() & Cpu::STATE_RUNNING)
            result |= 1UL << i;
    }

    running_mask_cached_.set((long)result);
    return result;
}

void CpuTable::reset()
{
    /* An exited task's last reference is dropped by its CPU's idle loop
       (TaskQueue::reap_exited), and once the halt has begun no CPU idles
       again: the others are parked and this one is on its way out. Dropped
       here instead, or everything that exited since each CPU last idled
       leaks -- the soft IRQ tasks among them, stopped moments before the
       halt. Before the table lock: freeing a task frees its stack, and the
       TLB shootdown for that reads the running CPUs under this lock. */
    for (ulong i = 0; i < stdlib::array_size(cpu_array_); i++)
        cpu_array_[i].get_task_queue().reap_exited();

    stdlib::AutoLock lock(lock_);

    for (ulong i = 0; i < stdlib::array_size(cpu_array_); i++)
    {
        auto& cpu = cpu_array_[i];
        cpu.reset();
    }
}

bool Cpu::run(Task::Func func, void *ctx)
{
    char name[TASK_NAME_LEN];
    stdlib::sn_printf(name, sizeof(name), "idle%lu", index_);
    idle_task_ptr_ = mm::talloc<Task, TAG>(name);
    if (idle_task_ptr_ == nullptr)
    {
        return false;
    }

    idle_task_ptr_->set_cpu_affinity(1UL << index_);
    idle_task_ptr_->set_idle();

    return idle_task_ptr_->run(task_queue_, func, ctx);
}

}