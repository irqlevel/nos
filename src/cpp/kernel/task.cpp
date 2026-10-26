#include "task.h"
#include "stack_probe.h"
#include "trace.h"
#include <hal/cpu.h>
#include <hal/context.h>
#include <hal/barrier.h>
#include "sched.h"
#include "cpu.h"
#include "sched.h"
#include "preempt.h"
#include "mutex.h"
#include "stack_trace.h"
#include "symtab.h"
#include <mm/new.h>

namespace kernel
{

Task::Task()
    : run_state_(RUN_QUEUED)
    , wake_next_(nullptr)
    , queued_at_(0)
    , stall_reported_(false)
    , task_queue_(nullptr)
    , rsp_(0)
    , state_(0)
    , flags_(0)
    , sleep_until_(0)
    , wake_time_(0)
    , switched_out_runnable_(false)
    , prev_(nullptr)
    , magic_(TASK_MAGIC)
    , cpu_affinity_(~(0UL))
    , pid_(INVALID_OBJECT_ID)
    , stack_ptr_(nullptr)
    , function_(nullptr)
    , ctx_(nullptr)
{
    ref_counter_.set(1);
    list_entry_.init();
    sleep_entry_.init();
    name_[0] = '\0';
}

Task::Task(const char* name)
    : Task()
{
    /* Cut as a format's output is, marked where it is cut */
    stdlib::sn_printf(name_, stdlib::array_size(name_), "%s", name);
    trace(TASK_LL, "task 0x%p %s", this, name_);
}

Task::~Task()
{
    BUG_ON(task_queue_ != nullptr);
    BUG_ON(stack_ptr_ != nullptr);
    BUG_ON(!sched_.entry.is_empty());
    BUG_ON(!sleep_entry_.is_empty());
    trace(TASK_LL, "task 0x%p %s dtor", this, name_);
}

void Task::release()
{
    BUG_ON(task_queue_ != nullptr);

    if (stack_ptr_ != nullptr)
    {
        trace(TASK_LL, "task 0x%p %s free stack 0x%lX",
            this, name_, (ulong)stack_ptr_);
        delete stack_ptr_;
        stack_ptr_ = nullptr;
    }
}

void Task::get()
{
    BUG_ON(ref_counter_.get() <= 0);
    ref_counter_.inc();
}

void Task::put()
{
    BUG_ON(ref_counter_.get() <= 0);
    if (ref_counter_.dec_and_test())
    {
        release();
        delete this;
    }
}

void Task::set_stopping()
{
    flags_.set_bit(FLAG_STOPPING_BIT);
}

bool Task::is_stopping()
{
    return flags_.test_bit(FLAG_STOPPING_BIT);
}

void Task::set_idle()
{
    flags_.set_bit(FLAG_IDLE_BIT);
}

void Task::clear_idle()
{
    flags_.clear_bit(FLAG_IDLE_BIT);
}

bool Task::is_idle()
{
    return flags_.test_bit(FLAG_IDLE_BIT);
}

void Task::set_idling(bool idling)
{
    if (idling)
        flags_.set_bit(FLAG_IDLING_BIT);
    else
        flags_.clear_bit(FLAG_IDLING_BIT);
}

bool Task::is_idling()
{
    return flags_.test_bit(FLAG_IDLING_BIT);
}

/* The sleeper's half of the handshake every wait here makes: the state goes
   up first, and the task looks for its work only after -- a locked
   operation, sequentially consistent, so the look cannot be made before the
   state is seen by a waker, who publishes the work first and looks at the
   state after (Unblock). A wakeup landing anywhere in between is either
   found by the look or finds the task Blocking, and puts it back. */
void Task::block()
{
    /* The task itself, running: Queued, and nobody else moves it from there */
    long was = run_state_.cmpxchg(RUN_BLOCKING, RUN_QUEUED);
    BUG_ON(was != RUN_QUEUED);
}

/* The waker's half: whatever it finds, a locked operation -- a failed
   compare-and-swap included -- so that the work it published before is
   ordered before the look, which a plain read would not be on arm64.
   Blocking: the task never left its CPU, and the Schedule() that would take
   it off finds it Queued and keeps it runnable. Sleeping: it left, and this
   call, having won it from any other waker and from its deadline, hands it
   back to its queue. Queued or Waking: someone was first, and nothing is
   left to do. A Sleeping task stays on the queue it slept on -- nothing
   moves one -- so the queue read here is the one it is waiting on. */
void Task::unblock()
{
    long was = run_state_.cmpxchg(RUN_QUEUED, RUN_BLOCKING);
    if (was != RUN_SLEEPING)
        return;

    if (run_state_.cmpxchg(RUN_WAKING, RUN_SLEEPING) != RUN_SLEEPING)
        return;

    wake_time_.set((long)get_boot_time().get_value());
    task_queue_->push_wake(this);
}

bool Task::is_blocked()
{
    long state = run_state_.get();
    return state == RUN_BLOCKING || state == RUN_SLEEPING;
}

void Task::exit()
{
    BUG_ON(this != get_current_task());

    exit_time_ = get_boot_time();
    TaskTable::get_instance().remove(this);
    state_.set(STATE_EXITED);

    schedule();

    PANIC("Can't be here");
}

void Task::exec_callback()
{
    Task* curr = get_current_task();
    if (this != curr)
    {
        diagnose_get_current_task();
        trace(0, "ExecCallback: this 0x%lX GetCurrentTask 0x%lX rsp 0x%lX",
            (ulong)this, (ulong)curr, hal::get_sp());
        BUG_ON(true);
    }
    start_time_ = get_boot_time();
    function_(ctx_);
    exit();
}

void Task::exec(void *task)
{
    static_cast<Task *>(task)->exec_callback();
}

/* Waits that Wait has reported (GetJoinReportCount) */
static Atomic join_reports;

void Task::wait(ulong report_ns)
{
    const ulong start = get_boot_time().get_value();
    ulong report_at = report_ns;

    while (state_.get() != STATE_EXITED)
    {
        sleep(1 * consts::NANO_SECS_IN_MS);

        ulong waited = get_boot_time().get_value() - start;
        if (waited < report_at)
            continue;

        report_join(waited);
        report_at = waited + stdlib::min(waited, JOIN_REPORT_MAX_NS);
    }
}

long Task::get_join_report_count()
{
    return join_reports.get();
}

/* Locate, one at a time across the machine: what it leaves with each CPU it
   asks (TaskProbe) is the next asker's to reuse, and a report's lines stay
   together */
static Mutex& locate_lock()
{
    static Mutex lock;
    return lock;
}

static void task_probe_ipi(void* ctx, Context* ipi_ctx);

/* What Locate leaves with a CPU it asks whether it is running a task. One
   per CPU, made the first time that CPU is asked and kept for good: a CPU
   with interrupts off never takes the IPI, and Locate stops waiting for it,
   so the IPI queued there -- and the answer its handler writes -- has to
   outlive the call that queued it. On the asker's stack, a CPU that took
   it late would write into whatever that stack had become by then. Under
   LocateLock, but for the answer, which the handler writes before it brings
   the IPI's Completion back to 0. */
struct TaskProbe final
{
    TaskProbe()
        : ipi(task_probe_ipi, this)
        , want(nullptr)
        , found(false)
        , pc(0)
        , count(0)
        , frames{}
        , pending(false)
    {
        /* Nothing asked yet, which is the same as answered */
        ipi.completion.done();
    }

    IPITask ipi;
    Task* want;
    bool found;
    ulong pc;
    size_t count;
    ulong frames[Task::WHERE_FRAMES];
    /* Sent, and not answered when Locate stopped waiting */
    bool pending;

    static const ulong TAG = 'TPrb';
};

static TaskProbe* task_probes[MAX_CPUS];

/* The IPI, on the CPU asked: whether the task it landed on -- the one whose
   stack the handler runs on, since neither arch switches stacks for an
   interrupt taken in the kernel -- is the task wanted, and if it is, where
   it was */
static void task_probe_ipi(void* ctx, Context* ipi_ctx)
{
    auto* probe = static_cast<TaskProbe*>(ctx);
    probe->found = (Task::try_get_current_task() == probe->want);
    if (!probe->found)
        return;

    probe->pc = ipi_ctx->get_ret_rip();
    probe->count = StackTrace::capture_from_sp(ipi_ctx->get_frame_pointer(),
        ipi_ctx->get_orig_rsp(), probe->frames, stdlib::array_size(probe->frames));
}

/* Asks every other CPU running whether it is running task, and waits up to
   answerNs for the answers. True when one was -- where says which,
   and where it was -- and when none was but some CPU did not answer: the
   task is on one of those, as far as anything can tell. False when every
   CPU answered no: it has left its CPU since it was seen on one. */
static bool locate_running(Task* task, Task::Where& where, ulong answer_ns)
{
    auto& cpus = CpuTable::get_instance();
    ulong ask = cpus.get_running_cpus_no_lock() & ~(1UL << cpus.get_current_cpu_id());
    ulong asked = 0;
    ulong silent = 0;

    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        if (!(ask & (1UL << i)))
            continue;

        if (task_probes[i] == nullptr)
            task_probes[i] = mm::talloc<TaskProbe, TaskProbe::TAG>();

        TaskProbe* probe = task_probes[i];
        if (probe == nullptr)
            continue;

        if (probe->pending)
        {
            if (probe->ipi.completion.get_counter() != 0)
            {
                /* Still has not taken the one asked before */
                silent |= 1UL << i;
                continue;
            }
            probe->pending = false;
        }

        probe->want = task;
        probe->found = false;
        probe->pc = 0;
        probe->count = 0;
        probe->ipi.completion.add(1);
        cpus.get_cpu(i).queue_ipi_task_async(probe->ipi);
        asked |= 1UL << i;
    }

    const ulong deadline = get_boot_time().get_value() + answer_ns;
    bool found = false;

    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        if (!(asked & (1UL << i)))
            continue;

        TaskProbe* probe = task_probes[i];
        while (probe->ipi.completion.get_counter() != 0 &&
               get_boot_time().get_value() < deadline)
        {
            sleep(consts::NANO_SECS_IN_MS);
        }

        if (probe->ipi.completion.get_counter() != 0)
        {
            probe->pending = true;
            silent |= 1UL << i;
            continue;
        }

        if (probe->found && !found)
        {
            found = true;
            where.what = Task::Where::Running;
            where.cpu = i;
            where.pc = probe->pc;
            where.count = probe->count;
            stdlib::mem_cpy(where.frames, probe->frames, probe->count * sizeof(ulong));
        }
    }

    if (found)
        return true;

    if (silent == 0)
        return false;

    where.what = Task::Where::Silent;
    where.silent_cpus = silent;
    return true;
}

bool Task::locate_switched(Where& where)
{
    if (stack_ptr_ == nullptr)
        return false;

    /* A seqlock's read, with the task's own switches for the sequence: what
       is read off its stack between the two looks is what its last switch
       left there only if no switch in or out came between them. Switched
       back in and still running, its State says so; switched in and out
       again, its count does. The stack stays mapped through all of it --
       the caller holds a reference, and Release frees it on the last -- and
       a stale stack pointer or frame chain is kept inside it by the bounds,
       so a look that loses the race names wrong frames and is thrown away,
       and never faults. */
    long switches = context_switches_.get();
    ulong base = (ulong)stack_ptr_;
    ulong limit = (ulong)&stack_ptr_->stack_top[0];
    ulong sp = __atomic_load_n(&rsp_, __ATOMIC_RELAXED);

    size_t count = StackTrace::capture_from(hal::task_saved_frame_pointer(sp, base, limit),
        base, limit, where.frames, WHERE_FRAMES);

    hal::smp_rmb();
    if (state_.get() != STATE_WAITING || context_switches_.get() != switches)
        return false;

    where.what = Where::Switched;
    where.blocked = is_blocked();
    where.asleep = where.blocked && sleep_until_.get() != 0;
    where.waking = (run_state_.get() == RUN_WAKING);
    where.count = count;
    return true;
}

void Task::locate_locked(Where& where, ulong answer_ns)
{
    where.what = Where::Unknown;
    where.blocked = false;
    where.asleep = false;
    where.waking = false;
    where.cpu = 0;
    where.pc = 0;
    where.silent_cpus = 0;
    where.answer_ns = answer_ns;
    where.count = 0;

    if (this == try_get_current_task())
    {
        where.what = Where::Running;
        where.cpu = CpuTable::get_instance().get_current_cpu_id();
        where.count = StackTrace::capture(where.frames, WHERE_FRAMES);
        return;
    }

    for (ulong attempt = 0; attempt < LOCATE_ATTEMPTS; attempt++)
    {
        long state = state_.get();
        if (state == STATE_EXITED)
        {
            where.what = Where::Exited;
            return;
        }

        if (state == STATE_WAITING)
        {
            if (locate_switched(where))
                return;
        }
        else if (state == STATE_RUNNING)
        {
            if (locate_running(this, where, answer_ns))
                return;
        }
        else
        {
            /* Not started */
            return;
        }
    }
}

void Task::locate(Where& where, ulong answer_ns)
{
    stdlib::AutoLock lock(locate_lock());
    locate_locked(where, answer_ns);
}

void Task::Where::describe(char* buf, size_t size)
{
    char symbol[SymbolTable::DESCRIBE_MAX];

    switch (what)
    {
    case Exited:
        stdlib::sn_printf(buf, size, "exited");
        break;
    case Switched:
        stdlib::sn_printf(buf, size, "%s",
            asleep ? "asleep" : (blocked ? "blocked" :
            (waking ? "woken, and not yet back on its cpu's run list" : "runnable, waiting for a cpu")));
        break;
    case Running:
        if (pc != 0 && SymbolTable::get_instance().describe(pc, symbol, sizeof(symbol)))
            stdlib::sn_printf(buf, size, "running on cpu %lu at %s", cpu, symbol);
        else if (pc != 0)
            stdlib::sn_printf(buf, size, "running on cpu %lu at 0x%lX", cpu, pc);
        else
            stdlib::sn_printf(buf, size, "running on cpu %lu", cpu);
        break;
    case Silent:
        stdlib::sn_printf(buf, size, "on a cpu that would not answer: cpus 0x%lX took no interrupt in %lu ms",
            silent_cpus, answer_ns / consts::NANO_SECS_IN_MS);
        break;
    case Unknown:
    default:
        stdlib::sn_printf(buf, size, "not started, or never still long enough to be read");
        break;
    }
}

void Task::report_join(ulong waited_ns)
{
    join_reports.inc();

    stdlib::AutoLock lock(locate_lock());

    Where where;
    locate_locked(where, LOCATE_ANSWER_NS);

    char what[2 * SymbolTable::DESCRIBE_MAX];
    where.describe(what, sizeof(what));

    Task* waiter = try_get_current_task();
    trace(0, "join: %s (pid %lu) has waited %lu ms for %s (pid %lu) to exit, and it is %s",
        (waiter != nullptr) ? waiter->get_name() : "?",
        (waiter != nullptr) ? waiter->pid_ : INVALID_OBJECT_ID,
        waited_ns / consts::NANO_SECS_IN_MS, get_name(), pid_, what);

    auto& symtab = SymbolTable::get_instance();
    char frame[SymbolTable::DESCRIBE_MAX];
    for (size_t i = 0; i < where.count; i++)
    {
        if (symtab.describe_return(where.frames[i], frame, sizeof(frame)))
            trace(0, "join:   [%lu] 0x%lX %s", (ulong)i, where.frames[i], frame);
        else
            trace(0, "join:   [%lu] 0x%lX", (ulong)i, where.frames[i]);
    }
}

bool Task::prepare_start(Func func, void* ctx)
{
    BUG_ON(stack_ptr_ != nullptr);
    BUG_ON(function_ != nullptr);

    stack_ptr_ = mm::talloc<Stack, TAG>(this);
    if (stack_ptr_ == nullptr)
    {
        return false;
    }

    /* Before the task runs, so what survives the fill is exactly what it
       never touched. */
    stack_probe::poison(&stack_ptr_->stack_bottom[0], sizeof(stack_ptr_->stack_bottom));

    trace(TASK_LL, "task 0x%p %s stack 0x%lX top 0x%lX",
        this, name_, (ulong)stack_ptr_, (ulong)&stack_ptr_->stack_top[0]);

    if (!TaskTable::get_instance().insert(this))
    {
        delete stack_ptr_;
        stack_ptr_ = nullptr;
        return false;
    }

    function_ = func;
    ctx_ = ctx;

    return true;
}

bool Task::start(Func func, void* ctx)
{
    if (!prepare_start(func, ctx))
        return false;

    rsp_ = hal::build_task_frame((ulong)&stack_ptr_->stack_top[0],
        (ulong)&Task::exec, (ulong)this);

    start_time_ = get_boot_time();
    state_.set(STATE_WAITING);

    auto task_queue = select_next_task_queue();
    if (task_queue == nullptr)
        return false;
    task_queue->insert(this);
    return true;
}

bool Task::run(class TaskQueue& task_queue, Func func, void* ctx)
{
    if (!prepare_start(func, ctx))
        return false;

    start_time_ = get_boot_time();
    run_start_time_ = get_boot_time();
    state_.set(STATE_RUNNING);

    task_queue.insert(this);

    /* Never returns: Run is only used for the idle/boot task bodies */
    hal::run_on_stack((ulong)&stack_ptr_->stack_top[0], function_, ctx_);
}

Task* Task::get_current_task()
{
    /* Derive the task from RSP by rounding down to StackSize boundary.
       Returns nullptr if the stack doesn't look valid (e.g. during SwitchComplete
       before the new stack is fully set up, or on a non-task stack).
       NOTE: no Trace() calls here — Trace takes a lock which calls
       PreemptDisable -> GetCurrentTask, causing infinite recursion. */
    ulong rsp = hal::get_sp();
    struct Stack* stack_ptr = reinterpret_cast<struct Stack *>(rsp & (~(STACK_SIZE - 1)));
    if (BUG_ON(stack_ptr->magic1 != STACK_MAGIC1))
        return nullptr;

    if (BUG_ON(stack_ptr->magic2 != STACK_MAGIC2))
        return nullptr;

    if (BUG_ON(rsp < ((ulong)&stack_ptr->stack_bottom[0] + consts::PAGE_SIZE)))
        return nullptr;

    if (BUG_ON(rsp > (ulong)&stack_ptr->stack_top[0]))
        return nullptr;

    Task* task = stack_ptr->task;
    if (BUG_ON(task->magic_ != TASK_MAGIC))
        return nullptr;

    return task;
}

Task* Task::try_get_current_task()
{
    /*
     * Same as GetCurrentTask but without BugOn — returns nullptr
     * on any invalid state. Safe to call from panic context.
     */
    ulong rsp = hal::get_sp();
    struct Stack* stack_ptr = reinterpret_cast<struct Stack *>(rsp & (~(STACK_SIZE - 1)));
    if (stack_ptr->magic1 != STACK_MAGIC1 || stack_ptr->magic2 != STACK_MAGIC2)
        return nullptr;

    if (rsp < ((ulong)&stack_ptr->stack_bottom[0] + consts::PAGE_SIZE))
        return nullptr;

    if (rsp > (ulong)&stack_ptr->stack_top[0])
        return nullptr;

    Task* task = stack_ptr->task;
    if (task == nullptr || task->magic_ != TASK_MAGIC)
        return nullptr;

    return task;
}

void Task::diagnose_get_current_task()
{
    /* Safe to call from contexts where Trace is allowed (e.g. Schedule).
       Re-checks the same conditions as GetCurrentTask but logs the failure. */
    ulong rsp = hal::get_sp();
    struct Stack* stack_ptr = reinterpret_cast<struct Stack *>(rsp & (~(STACK_SIZE - 1)));

    trace(0, "DiagTask: rsp 0x%lX base 0x%lX", rsp, (ulong)stack_ptr);
    trace(0, "DiagTask: Magic1 0x%lX expect 0x%lX", stack_ptr->magic1, STACK_MAGIC1);
    trace(0, "DiagTask: Magic2 0x%lX expect 0x%lX", stack_ptr->magic2, STACK_MAGIC2);
    trace(0, "DiagTask: StackBottom+page 0x%lX StackTop 0x%lX",
        (ulong)&stack_ptr->stack_bottom[0] + consts::PAGE_SIZE,
        (ulong)&stack_ptr->stack_top[0]);

    if (stack_ptr->magic1 == STACK_MAGIC1 && stack_ptr->magic2 == STACK_MAGIC2)
    {
        Task* task = stack_ptr->task;
        trace(0, "DiagTask: Task 0x%lX TaskMagic 0x%lX expect 0x%lX",
            (ulong)task, task->magic_, TASK_MAGIC);
    }
}

void Task::set_name(const char *fmt, ...)
{
    name_[0] = '\0';

    va_list args;
    va_start(args, fmt);
    stdlib::vsn_printf(name_, stdlib::array_size(name_), fmt, args);
    va_end(args);
}

const char* Task::get_name()
{
    return name_;
}

void Task::update_runtime()
{
    auto now = get_boot_time();
    runtime_ += (now - run_start_time_);
    run_start_time_ = now;
}

void Task::set_cpu_affinity(ulong affinity)
{
    stdlib::AutoLock lock(lock_);
    cpu_affinity_ = affinity;
}

ulong Task::get_cpu_affinity()
{
    stdlib::AutoLock lock(lock_);
    return cpu_affinity_;
}

TaskQueue* Task::select_next_task_queue()
{
    /* The mirror, not the locked walk: this runs on every context switch,
       and GetRunningCpus takes the table lock and then each of the 64 CPUs'
       own locks to read one flag apiece. Nothing ever clears StateRunning,
       so the two answers agree once a CPU has published itself. */
    ulong cpu_mask = CpuTable::get_instance().get_running_cpus_no_lock() & cpu_affinity_;
    if (cpu_mask == 0)
        return nullptr;

    /* Read once: Remove() clears it, and the caller may be about to. */
    class TaskQueue* current = task_queue_;

    class TaskQueue* best = nullptr;
    long best_load = 0;
    long best_count = 0;

    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        if (!(cpu_mask & (1UL << i)))
            continue;

        /* What could run there is what a task placed there competes with:
           tasks asleep on a CPU cost it nothing. They used to be counted
           like any other, and a CPU home to a dozen sleepers looked busier
           than one with two tasks spinning. Of two CPUs as busy, the one
           home to fewer tasks, so that new ones -- most of which sleep --
           spread out rather than all go where the first CPU is. */
        auto& cand = CpuTable::get_instance().get_cpu(i).get_task_queue();
        long load = cand.get_runnable_count();
        long count = cand.get_task_count();

        /* The queue this task is already on competes like any other. It used
           to be skipped outright, which meant every task was moved to another
           CPU on every context switch -- not balancing, just motion, and it
           threw away whatever the task had warm in that core's caches. */
        if (best == nullptr || load < best_load || (load == best_load && count < best_count))
        {
            best = &cand;
            best_load = load;
            best_count = count;
        }
    }

    if (best == nullptr)
        return nullptr;

    /* Placing a task for the first time: there is nowhere to stay. */
    if (current == nullptr)
        return best;

    if (best == current)
        return nullptr;

    /* This task is runnable and still counted on its own queue, so moving
       it takes one off that side and puts one on the other: a difference of
       one would only be reversed on the next switch, and the task would
       trade places forever. Move for two. */
    if (best_load + MIGRATE_THRESHOLD > current->get_runnable_count())
        return nullptr;

    return best;
}

TaskTable::TaskTable()
{
}

TaskTable::~TaskTable()
{
    for (size_t i = 0; i < stdlib::array_size(task_list_); i++)
    {
        stdlib::ListEntry task_list;

        {
            stdlib::AutoLock lock(lock_[i]);
            task_list.move_tail_list(&task_list_[i]);
        }

        while (!task_list.is_empty())
        {
            Task* task = CONTAINING_RECORD(task_list.remove_head(), Task, table_list_entry_);
            task->table_list_entry_.init();
            task->put();
        }
    }
}

bool TaskTable::insert(Task *task)
{
    ObjectId pid = task_object_table_.insert(task);
    if (pid == INVALID_OBJECT_ID)
    {
        return false;
    }
    task->pid_ = pid;

    task->get();
    size_t i = stdlib::hash_ptr(task) % stdlib::array_size(task_list_);
    stdlib::AutoLock lock(lock_[i]);

    BUG_ON(!task->table_list_entry_.is_empty());
    task_list_[i].insert_tail(&task->table_list_entry_);

    return true;
}

void TaskTable::remove(Task *task)
{
    {
        size_t i = stdlib::hash_ptr(task) % stdlib::array_size(task_list_);

        task_object_table_.remove(task->pid_);

        stdlib::AutoLock lock(lock_[i]);

        BUG_ON(task->table_list_entry_.is_empty());
        task->table_list_entry_.remove_init();
    }

    task->put();
}

Task* TaskTable::lookup(ulong pid)
{
    return reinterpret_cast<Task*>(task_object_table_.lookup(pid));
}

size_t TaskTable::sample_cpu(CpuSample* out, size_t max)
{
    size_t n = 0;

    for (size_t i = 0; i < stdlib::array_size(task_list_) && n < max; i++)
    {
        stdlib::AutoLock lock(lock_[i]);

        size_t walked = 0;
        for (auto curr_entry = task_list_[i].flink;
            curr_entry != &task_list_[i] && n < max;
            curr_entry = curr_entry->flink)
        {
            if (++walked > MAX_TASKS_PER_LIST)
            {
                trace(0, "SampleCpu: task list %lu does not end after %lu entries",
                    (ulong)i, (ulong)MAX_TASKS_PER_LIST);
                break;
            }

            Task* task = CONTAINING_RECORD(curr_entry, Task, table_list_entry_);

            out[n].pid = task->pid_;
            out[n].runtime_ns = task->runtime_.get_value();
            stdlib::mem_cpy(out[n].name, task->get_name(), sizeof(out[n].name));
            out[n].name[sizeof(out[n].name) - 1] = '\0';
            n++;
        }
    }

    return n;
}

ulong Task::get_stack_free()
{
    if (stack_ptr_ == nullptr)
        return 0;

    return stack_probe::untouched(&stack_ptr_->stack_bottom[0],
        sizeof(stack_ptr_->stack_bottom));
}

void TaskTable::stacks(stdlib::Printer& printer, ulong& worst_free)
{
    for (size_t i = 0; i < stdlib::array_size(task_list_); i++)
    {
        stdlib::AutoLock lock(lock_[i]);

        size_t walked = 0;
        for (auto curr_entry = task_list_[i].flink;
            curr_entry != &task_list_[i];
            curr_entry = curr_entry->flink)
        {
            if (++walked > MAX_TASKS_PER_LIST)
            {
                printer.printf("task list %lu does not end after %lu entries\n",
                    (ulong)i, (ulong)MAX_TASKS_PER_LIST);
                break;
            }

            Task* task = CONTAINING_RECORD(curr_entry, Task, table_list_entry_);
            ulong free = task->get_stack_free();
            if (free == 0)
                continue;

            printer.printf("task %lu %lu %lu %s\n", task->pid_,
                (ulong)Task::STACK_SIZE - free, (ulong)Task::STACK_SIZE, task->get_name());

            if (free < worst_free)
                worst_free = free;
        }
    }
}

void TaskTable::ps(stdlib::Printer& printer)
{
    printer.printf("pid state flags runtime ctxswitches name\n");

    for (size_t i = 0; i < stdlib::array_size(task_list_); i++)
    {
        stdlib::AutoLock lock(lock_[i]);

        size_t walked = 0;
        for (auto curr_entry = task_list_[i].flink;
            curr_entry != &task_list_[i];
            curr_entry = curr_entry->flink)
        {
            if (++walked > MAX_TASKS_PER_LIST)
            {
                printer.printf("task list %lu does not end after %lu entries\n",
                    (ulong)i, (ulong)MAX_TASKS_PER_LIST);
                trace(0, "Ps: task list %lu does not end after %lu entries",
                    (ulong)i, (ulong)MAX_TASKS_PER_LIST);
                break;
            }

            Task* task = CONTAINING_RECORD(curr_entry, Task, table_list_entry_);
            ulong flags = (ulong)task->flags_.get();
            if (task->is_blocked())
                flags |= 1UL << Task::FLAG_BLOCKED_BIT;
            printer.printf("%lu %lu 0x%lX %lu.%lu %lu %s\n",
                task->pid_, task->state_.get(), flags, task->runtime_.get_secs(),
                task->runtime_.get_usecs(), task->context_switches_.get(), task->get_name());
        }
    }
}

}