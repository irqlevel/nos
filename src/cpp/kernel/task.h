#pragma once

#include <lib/stdlib.h>
#include <lib/list_entry.h>
#include <lib/printer.h>

#include "atomic.h"
#include "forward.h"
#include "spin_lock.h"
#include "panic.h"
#include "object_table.h"
#include "eevdf.h"

namespace kernel
{

/* Task name buffer, shared by Task and by TaskTable::CpuSample. */
static const size_t TASK_NAME_LEN = 32;

class Task final : public Object
{
public:

    /* 64 KB. Eight pages were enough until the kernel grew a TLS client:
       verifying a certificate chain walks candidate issuers recursively and
       goes several times deeper than anything else a task does, and a task
       stack that overflows takes the machine down with it. */
    static const ulong STACK_SIZE = 16 * consts::PAGE_SIZE;
    static const ulong STACK_MAGIC1 = 0xBCDEBCDE;
    static const ulong STACK_MAGIC2 = 0xCBDECBDE;

    static const ulong TASK_MAGIC = 0xCBDECBEF;

    struct Stack final
    {
        Stack(Task* task)
            : task(task)
            , magic1(STACK_MAGIC1)
            , magic2(STACK_MAGIC2)
        {
        }

        ~Stack()
        {
            BUG_ON(magic1 != STACK_MAGIC1);
            BUG_ON(magic2 != STACK_MAGIC2);
            magic1 = 0;
            magic2 = 0;
            task = nullptr;
        }

        Task* task;
        ulong magic1;
        u8 stack_bottom[STACK_SIZE - 3 * sizeof(ulong)];
        u8 stack_top[0];
        ulong magic2;

    private:
        Stack(const Stack& other) = delete;
        Stack(Stack&& other) = delete;
        Stack& operator=(const Stack& other) = delete;
        Stack& operator=(Stack&& other) = delete;
    } __attribute__((packed));

    static_assert(sizeof(Stack) == STACK_SIZE, "Invalid size");

    using Func = void (*)(void *ctx);

    Task();

    /* A task named name, as it is. Not a format: a task is made through
       TAlloc, and a format handed on through a template reaches the
       constructor as a variable no compiler can check against its
       arguments -- "idle%u" was given a ulong that way, which nothing
       saw. A name to be formatted is made with SnPrintf first, where the
       check is. */
    explicit Task(const char* name);

    bool run(class TaskQueue& task_queue, Func func, void* ctx);

    static Task* get_current_task();
    static Task* try_get_current_task();
    static void diagnose_get_current_task();

    virtual void get() override;
    virtual void put() override;

    bool start(Func func, void* ctx);

    /* Waits for the task to exit, and says so when that takes long: once
       reportNs have passed -- who waits, for which task, for how long, and
       where that task is (Locate) -- and again each time the wait has
       doubled, or JoinReportMaxNs on, whichever is sooner. It never gives
       up: a task that has not exited is still using whatever its waiter is
       about to free. A join that never ends is a stop, an unload or a
       shutdown that never ends, and one that says nothing is the worst
       outcome there is: the report is the TLB shootdown's naming of the
       CPUs that never answered, for the tasks every stop path waits on. */
    void wait(ulong report_ns = JOIN_REPORT_NS);

    static const ulong JOIN_REPORT_NS = 10 * consts::NANO_SECS_IN_SEC;
    static const ulong JOIN_REPORT_MAX_NS = 300 * consts::NANO_SECS_IN_SEC;

    /* Waits that Wait has reported, since boot */
    static long get_join_report_count();

    static const size_t WHERE_FRAMES = 16;

    /* Where a task is, as Locate found it */
    struct Where
    {
        enum Kind : ulong
        {
            /* Not started, or never still long enough to be read */
            Unknown,
            Exited,
            /* Off every CPU: blocked, asleep, or runnable and waiting for
               one. Frames as its switch left them. */
            Switched,
            /* On Cpu, where the interrupt that asked it landed: at Pc,
               Frames from there. The asker's own task too, Pc 0. */
            Running,
            /* Not off its CPU, and SilentCpus took no interrupt in
               AnswerNs to say whether it is on theirs: interrupts off
               there, which is where such a task usually is. */
            Silent,
        };

        Kind what;
        bool blocked;
        bool asleep;
        /* Woken (RunWaking), and not yet put back on its queue's run list:
           a moment's state, and a task found in it for long is a wakeup
           its CPU has not taken */
        bool waking;
        ulong cpu;
        ulong pc;
        ulong silent_cpus;
        ulong answer_ns;
        size_t count;
        ulong frames[WHERE_FRAMES];

        /* What, in words, into buf: "asleep", "running on cpu 3 at
           Spin+0x1c", ... */
        void describe(char* buf, size_t size);
    };

    /* Where this task is, now: its return addresses, innermost first, and
       for a task on a CPU which one and where it was. A task off its CPU is
       read off its stack, and held to not having run while it was read; one
       on a CPU is asked, by an IPI to every other CPU, whose answers are
       waited for up to LocateAnswerNs -- a CPU with interrupts off takes no
       IPI, and is named instead. From task context, since it waits; one at
       a time across the machine. */
    void locate(Where& where, ulong answer_ns = LOCATE_ANSWER_NS);

    static const ulong LOCATE_ANSWER_NS = 1 * consts::NANO_SECS_IN_SEC;

    void set_stopping();
    bool is_stopping();

    PRINTF_FORMAT(2, 3) void set_name(const char *fmt, ...);
    const char* get_name();

    /* Bytes of this task's stack that the poison scan still finds intact --
       the part it has never reached. 0 when it has no stack yet. */
    ulong get_stack_free();

    void update_runtime();

    void set_cpu_affinity(ulong affinity);
    ulong get_cpu_affinity();

    /* How much lighter another CPU's queue has to be before a running task
       is moved to it. Two, because the task is still counted where it is. */
    static const long MIGRATE_THRESHOLD = 2;

    /* The queue this task should be on: the lightest one its affinity
       allows, or nullptr to say it is already on the right one. */
    void set_idle();
    /* Back to an ordinary task: scheduled in its turn, not only when
       nothing else on its queue can run (see TaskQueue::ReleaseIdle) */
    void clear_idle();
    bool is_idle();

    /* The idle task is the last resort only while it idles -- inside
       Cpu::Idle, about to halt or halted. Running anything else -- the
       boot, its self-tests, the shutdown -- it is a task like any other and
       takes its turn: it used to be the last resort whatever it ran, and a
       task the boot started on the boot CPU that never blocked kept the
       boot itself off that CPU for as long as it ran. Set and cleared by
       the idle task itself, read by its own CPU's scheduler. */
    void set_idling(bool idling);
    bool is_idling();

    /* A task that has asked not to be scheduled until someone wakes it.
       Block() is called by the task itself, immediately before the Schedule()
       that switches it out; Unblock() may be called from any CPU and from
       hard IRQ context. Both are locked operations on one word, RunState,
       and take no lock: an Unblock() that finds the task already off its
       CPU puts it back on its queue's run list by way of that queue's wake
       stack (TaskQueue::PushWake), which takes none either. */
    void block();
    void unblock();
    bool is_blocked();

    /* Where a task is with its queue -- RunState's values. Queued: it can
       run, and is on its queue's run list or on the CPU. Blocking: it has
       called Block() and is still on its CPU -- its Schedule() has not taken
       it off. Sleeping: that Schedule() has, and it is on no list but the
       sleep list, if it is asleep until a deadline. Waking: an Unblock()
       found it Sleeping and is handing it back (TaskQueue::PushWake). Each
       move is a compare-and-swap, so of an Unblock() and the Schedule() that
       takes the task off, or of a deadline and an Unblock(), exactly one
       wins: Blocking -> Queued is the waker's, Blocking -> Sleeping its
       CPU's, Sleeping -> Waking a waker's, Sleeping -> Queued the deadline's,
       Waking -> Queued its CPU's, as it puts it on the run list. */
    static const long RUN_QUEUED = 0;
    static const long RUN_BLOCKING = 1;
    static const long RUN_SLEEPING = 2;
    static const long RUN_WAKING = 3;

    TaskQueue* select_next_task_queue();

    static const long STATE_WAITING = 1;
    static const long STATE_RUNNING = 2;
    static const long STATE_EXITED = 3;

    static const long FLAG_STOPPING_BIT = 1;

    /* The CPU's idle task. Marked so the scheduler can treat it as the last
       resort it is, rather than as one more task to take a turn. */
    static const long FLAG_IDLE_BIT = 2;

    /* The idle task inside Cpu::Idle (SetIdling) */
    static const long FLAG_IDLING_BIT = 4;

    /* Shown in ps's flags while the task is blocked, as it always was; what
       says so is RunState */
    static const long FLAG_BLOCKED_BIT = 3;

public:
    /* On its queue's list of every task it is home to, or the exited list */
    stdlib::ListEntry list_entry_;
    stdlib::ListEntry table_list_entry_;

    /* What its queue's EEVDF knows of it -- its weight, slice, virtual
       runtime, deadline and lag -- and its place on the queue's run list
       (Sched.Entry) while it can run and is off the CPU; and its place on
       the sleep list while asleep until a deadline. All under the queue's
       lock; set from elsewhere through SetTaskNice and SetTaskSlice. */
    SchedEntity sched_;
    stdlib::ListEntry sleep_entry_;

    /* RunQueued, RunBlocking, RunSleeping or RunWaking (above) */
    Atomic run_state_;

    /* The next on the wake stack it is on (TaskQueue::PushWake): the
       waker's to write before the push publishes it, its queue's CPU's to
       read once it has taken the stack */
    Task* wake_next_;

    /* When it went on the run list, and whether a wait that long has been
       reported (TaskQueue::CheckStalls): under the queue's lock */
    ulong queued_at_;
    bool stall_reported_;

    TaskQueue* task_queue_;
    SpinLock lock_;
    Atomic preempt_disable_counter_;
    /* A reschedule that came while PreemptDisableCounter was up: the tick's,
       or the one an IPI asks for on behalf of a task just woken. The task
       keeps the CPU, as it must, and the reschedule is made the moment the
       count is back to zero (PreemptEnable), where it used to be dropped. */
    Atomic preempt_pending_;
    Atomic context_switches_;
    ulong rsp_;

    Atomic state_;
    Atomic flags_;
    /* The boot time, in nanoseconds, at which a task blocked in Sleep() is
       to run again: the scheduler's walk on its CPU notices that the time
       has come (TaskQueue::SelectNext) -- the tick's walk at the latest --
       and runs it, and the task unblocks itself. 0 while it is not asleep. */
    Atomic sleep_until_;

    /* When an Unblock() last took the flag down: the start of the wait for
       the CPU that SchedStats measures at the switch in. Written by the
       waker, from any CPU and any context. */
    Atomic wake_time_;

    /* It left the CPU able to run -- displaced, not blocked: the wait until
       it runs again is a delay, not a wakeup's (SchedStats). Under Lock. */
    bool switched_out_runnable_;

    stdlib::Time run_start_time_;
    stdlib::Time runtime_;
    stdlib::Time start_time_;
    stdlib::Time exit_time_;

    Task* prev_;
    ulong magic_;
    ulong cpu_affinity_;
    ulong pid_;

private:
    Task(const Task& other) = delete;
    Task(Task&& other) = delete;
    Task& operator=(const Task& other) = delete;
    Task& operator=(Task&& other) = delete;
    ~Task();

    void release();
    void exit();
    void exec_callback();
    static void exec(void *task);

    bool prepare_start(Func func, void* ctx);

    /* Wait's report, after waitedNs */
    void report_join(ulong waited_ns);

    /* Locate, under its lock; and its case of a task off its CPU, false if
       the task ran while being read */
    void locate_locked(Where& where, ulong answer_ns);
    bool locate_switched(Where& where);

    /* Looks a task Locate finds in neither place keeps getting: off its
       CPU when asked on one, on one when read off its stack */
    static const ulong LOCATE_ATTEMPTS = 4;

    Stack* stack_ptr_;
    Func function_;
    void* ctx_;
    Atomic ref_counter_;

    char name_[TASK_NAME_LEN];

    static const ulong TAG = 'Task';
};

class TaskTable final
{
public:
    static TaskTable& get_instance()
    {
        static TaskTable instance;
        return instance;
    }

    bool insert(Task *task);
    void remove(Task *task);

    Task* lookup(ulong pid);

    void ps(stdlib::Printer& printer);

    /* High-water marks of every task stack, worst free tracked in worstFree. */
    void stacks(stdlib::Printer& printer, ulong& worst_free);

    /* One task's accumulated CPU time, copied out under the bucket lock so
       the sample stays readable after the task itself is gone -- `top` takes
       two of these a moment apart and subtracts. */
    struct CpuSample
    {
        ulong pid;
        ulong runtime_ns;
        char name[TASK_NAME_LEN];
    };

    size_t sample_cpu(CpuSample* out, size_t max);

private:
    TaskTable(const TaskTable& other) = delete;
    TaskTable(TaskTable&& other) = delete;
    TaskTable& operator=(const TaskTable& other) = delete;
    TaskTable& operator=(TaskTable&& other) = delete;

    TaskTable();
    ~TaskTable();

    static const size_t TASK_LIST_COUNT = 512;

    /* Ps walks each bucket holding that bucket's lock with interrupts off. A
       damaged list -- a cycle, a half-written link -- turns that walk into an
       infinite loop on a CPU nobody can interrupt, holding a lock the
       scheduler wants, and the machine stops without a panic and without a
       reset. There is no legitimate way for one of 512 buckets to hold this
       many tasks, so passing it means the list is broken; say so and stop,
       which leaves a diagnosis instead of a brick. */
    static const size_t MAX_TASKS_PER_LIST = 4096;

    SpinLock lock_[TASK_LIST_COUNT];
    stdlib::ListEntry task_list_[TASK_LIST_COUNT];

    ObjectTable task_object_table_;
};

}