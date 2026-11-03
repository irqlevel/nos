#pragma once

#include <lib/stdlib.h>
#include <lib/printer.h>
#include <kernel/trace_ring.h>

#include "forward.h"

namespace kernel
{
namespace tp
{

/* Tracepoints: events a subsystem records at the moment they happen, each
   turned on and off while the kernel runs (`trace on sched_switch`), into a
   ring of the recording CPU's own (kernel/trace_ring.h) -- no lock, no
   allocation, interrupts off for the few dozen stores of one record -- and
   read out of every CPU's ring, merged by time, without stopping anything
   (`trace show`). docs/tracing.md is the long form.

   An event is a number (Id), a payload struct of at most
   TraceRing::PAYLOAD_BYTES, and a description in tracepoint.cpp's table --
   its name, group, help line and fields -- from which a record is printed.
   A tracepoint site is

       if (unlikely(tp::on(tp::Id::SchedSwitch)))
           tp_sched::sched_switch(...);

   so that one that is off costs a byte's load from a line every CPU only
   reads, and a branch not taken: its arguments are computed only inside. */

/* Every event, by number: its index into the descriptions and the flags.
   Appended to, never reordered -- a saved trace names events by number. */
enum class Id : u16
{
    /* The self-test's, and `trace test`'s and `trace bench`'s. */
    Test,

    /* The scheduler's (group "sched"): a decision and everything it does --
       who left a CPU, who took it, who was woken by whom. */
    SchedSwitch,
    SchedWakeup,
    SchedBlock,
    SchedMigrate,
    SchedNew,
    SchedExit,
    SchedIpi,
    SchedPreemptDeferred,
    /* Group "idle": a CPU about to halt, and back from it. */
    IdleEnter,
    IdleExit,
    /* Group "debug": what the scheduler is expensive enough to be worth a
       record of its own -- the slice armed on the timer. */
    SchedSlice,

    /* The hypervisor's (group "hv"), recorded by the hv module's Rust: a
       guest CPU's exits, what goes in at its entries, its halts and what
       woke it, its timer's interrupts, and the rings between its CPUs. */
    HvExit,
    HvInject,
    HvHalt,
    HvWake,
    HvTimer,
    HvRing,

    Count
};

const ulong COUNT = (ulong)Id::Count;

/* Whether each event is recorded: a byte each, written by `trace on` and
   `trace off` (task context, any CPU), read at every tracepoint. A stale
   read costs at most a record kept or lost as the flag moves. */
extern u8 enabled[COUNT];

static inline __attribute__((always_inline)) bool on(Id id)
{
    return __atomic_load_n(&enabled[(ulong)id], __ATOMIC_RELAXED) != 0;
}

/* The record of event `id`: `size` bytes of `payload`, exactly its
   description's size (BUG_ON otherwise -- a site that disagrees with its
   own event is a bug). Into this CPU's ring, with the cycle counter; nothing
   when there is no ring, or recording is frozen. Any context: interrupts
   are turned off for the write. Out of line, called only when on(). */
void emit(Id id, const void* payload, ulong size);

/* How a field is printed. */
enum class Kind : u8
{
    Unsigned,
    Signed,
    Hex,
    /* Nanoseconds, printed as microseconds. */
    Ns,
    Cpu,
    Pid,
    /* An index into the field's names. */
    Enum,
    /* A task's name: the field's bytes, up to the first zero, printed as a
       string -- what a task is called, not what it points at. */
    Comm,
};

struct Field
{
    const char* name;
    u8 offset;
    u8 size;
    Kind kind;
    /* Kind::Enum: what the values from 0 up are called. */
    const char* const* names;
    u8 name_count;
};

struct Event
{
    const char* name;
    const char* group;
    const char* help;
    u8 size;
    u8 field_count;
    const Field* fields;
};

/* The description of event `id`. */
const Event& event(Id id);

/* `trace on` and `trace off`, for every event `what` names -- an event's
   name, a group's, or `all`: how many it named, or -1 when it named none
   or the rings could not be made. Task context. */
long set(const char* what, bool turn_on, stdlib::Printer& con);

/* The events `tracepoints=` named at boot, turned on: once every CPU is up,
   so that each has its ring. */
void boot_on(const char* list);

/* The shell's `trace` command. */
void command(const char* args, stdlib::Printer& con);

/* The size of the rings made from now on, in KiB -- 64 times a power of two
   -- and the rings there are made again at it, what they held with them:
   false, and nothing changed, for a size not taken or no memory. Task
   context. */
bool set_ring_kib(ulong kib, stdlib::Printer& con);

/* The size rings are made at, in KiB. */
ulong ring_size_kib();

/* Every event off and every ring freed, once no CPU can be writing into
   one (CpuTable::synchronize). Task context. */
void free_rings();

/* The records of event `id` cpu's ring holds, oldest first, the newest
   `max` of them at most: how many went into `out`. The self-test's. */
ulong read_cpu(ulong cpu, Id id, TraceRing::Record* out, ulong max);

/* The self-test's: the payload `trace test` records. */
struct Test
{
    u64 seq;
    u64 value;
};

/* A task's name in a payload: what fits, no terminator needed -- the field's
   own size is where a reader stops, so a name longer than it is cut, never
   read past. */
const ulong COMM_BYTES = 12;

/* waker_pid of a wakeup nobody made -- a deadline's, from the sleep list */
const u32 NO_TASK_PID = 0xFFFFFFFF;

/* ---- the scheduler's payloads (the sites are sched.cpp, task.cpp, cpu.cpp:
   where each event is emitted, and why there, is in docs/tracing.md) ---- */

/* The states `prev_state` names: what a task left its CPU to be. */
enum class SwitchState : u8
{
    Runnable = 0, /* displaced while it could still run */
    Blocked = 1,  /* waiting on someone else's wakeup */
    Sleeping = 2, /* asleep until a deadline */
    Exited = 3,
};

struct SchedSwitch
{
    u32 prev_pid;
    char prev_comm[COMM_BYTES];
    u32 next_pid;
    char next_comm[COMM_BYTES];
    u32 next_cpu;
    u8 prev_state;
    u8 reason;
};

struct SchedWakeup
{
    u32 pid;
    char comm[COMM_BYTES];
    /* The CPU whose queue it went back to */
    u32 cpu;
    /* 1: its deadline came; 0: someone's unblock() */
    u8 deadline;
    /* The task that woke it, when woken from one of a task's stacks --
       from an interrupt's context it is the task the interrupt landed on */
    u32 waker_pid;
    /* A deadline's: how long past the deadline the sleep list was walked
       -- a timer late, a CPU with interrupts off. Its wait is counted from
       the deadline, as `sched` and the wakeup trigger count it; 0 for an
       unblock() */
    u32 late_ns;
};

struct SchedBlock
{
    u32 pid;
    char comm[COMM_BYTES];
    u32 cpu;
    /* 1: asleep until a deadline; 0: waiting on someone else's wakeup */
    u8 deadline;
    /* How long it was on the CPU this time */
    u32 ran_ns;
};

struct SchedMigrate
{
    u32 pid;
    char comm[COMM_BYTES];
    u32 from;
    u32 to;
};

struct SchedNew
{
    u32 pid;
    char comm[COMM_BYTES];
    u32 cpu;
};

struct SchedExit
{
    u32 pid;
    char comm[COMM_BYTES];
    u32 cpu;
};

struct SchedIpi
{
    u32 cpu;
    /* What queued work the interrupt found, a wakeup's reschedule or a TLB
       shootdown among it */
    u32 tasks;
};

struct SchedPreemptDeferred
{
    u32 pid;
    char comm[COMM_BYTES];
    u32 cpu;
    /* Its preempt_disable_counter_ as the reschedule found it */
    u32 count;
    u8 idle;
};

/* idle_enter and idle_exit: the CPU, and its idle task. */
struct Idle
{
    u32 cpu;
    u32 pid;
};

struct SchedSlice
{
    u32 cpu;
    u32 pid;
    /* How long from now the current one's slice ends */
    u32 slice_ns;
    /* The tasks the queue had that could run */
    u32 runnable;
};

/* ---- the hypervisor's payloads ----
   Recorded by the hv module's Rust (src/crates/hv: vm.rs, run.rs, smp.rs),
   which declares each in kcore/src/trace/events.rs: every boot holds those
   to the descriptions in tracepoint.cpp, a field's name, offset and size and
   an enumeration's names value by value, so the two are one layout. These
   are here for the descriptions' offsets; nothing in C++ fills one. A guest
   CPU is said by its number in its guest and its task's pid, which tells
   one VM's from another's. `pad` keeps each free of padding, which Rust's
   side has to be. */

struct HvExit
{
    u64 detail;
    u64 rip;
    u32 pid;
    u32 guest_ns;
    u32 out_ns;
    u32 since_ns;
    u32 info;
    u16 vcpu;
    u8 reason;
    u8 pad;
};

struct HvInject
{
    u32 pid;
    u16 vcpu;
    u8 vector;
    u8 kind;
};

struct HvHalt
{
    u32 pid;
    u32 wait_ns;
    u32 poll_ns;
    u16 vcpu;
    u8 until;
    u8 pad;
};

struct HvWake
{
    u32 pid;
    u32 halted_ns;
    u32 late_ns;
    u16 vcpu;
    u8 how;
    u8 pad;
};

struct HvTimer
{
    u32 pid;
    u32 late_ns;
    u16 vcpu;
    u8 how;
    u8 pad;
};

struct HvRing
{
    u32 pid;
    u16 vcpu;
    u8 how;
    u8 pad;
};

/* ---- the sites ----
   One function an event, building its payload off what the scheduler has in
   its hands, so that a site is a line: the check, and the call with the
   values it already has. Each is called only when its event is on, and the
   task.h it needs for the pid and the name is tracepoint.cpp's to include
   -- not this header's, which the scheduler's own includes read. */
void emit_sched_switch(Task* prev, Task* next, ulong cpu, u8 state, u8 reason);
void emit_sched_wakeup(Task* task, ulong cpu, bool deadline, Task* waker, ulong late_ns);
void emit_sched_block(Task* task, ulong cpu, ulong ran_ns);
void emit_sched_migrate(Task* task, ulong from, ulong to);
void emit_sched_new(Task* task, ulong cpu);
void emit_sched_exit(Task* task, ulong cpu);
void emit_sched_ipi(ulong cpu, ulong tasks);
void emit_sched_preempt_deferred(Task* task, ulong cpu, ulong count, bool idle);
void emit_idle(Id id, ulong cpu, Task* idle_task);
void emit_sched_slice(ulong cpu, Task* task, ulong slice_ns, ulong runnable);

/* The wakeup trigger: `trace freeze wakeup>N` arms it, and a task that gets
   its CPU later than that after a wakeup freezes the rings where they are,
   so that what led to the wait stays. The threshold in ns, 0 for none --
   read by the scheduler's own check (TaskQueue::note_run). */
extern u64 wakeup_trigger_ns;

inline bool wakeup_trigger_armed()
{
    return __atomic_load_n(&wakeup_trigger_ns, __ATOMIC_RELAXED) != 0;
}

/* A wakeup's wait, from where the scheduler has it in its hand: freezes the
   rings, once, if it is over the trigger's. */
void note_wakeup(ulong waited_ns, ulong now_ns, Task* task, ulong cpu);

}
}
