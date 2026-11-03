#include "tracepoint.h"
#include "trace_ring.h"
#include "trace_window.h"

#include "cpu.h"
#include "klog.h"
#include "mutex.h"
#include "panic.h"
#include "preempt.h"
#include "task.h"
#include "time.h"
#include <hal/barrier.h>
#include <hal/cpu.h>
#include <hal/irqchip.h>
#include <mm/new.h>

namespace kernel
{
namespace tp
{

u8 enabled[COUNT] = {};

/* `trace freeze wakeup>N`: a wakeup that waited longer than N ns freezes the
   rings where they are. 0 while none is armed; the scheduler reads it on
   every wakeup through wakeup_trigger_armed(), so it is tp's and not the
   file's. */
u64 wakeup_trigger_ns = 0;

namespace
{

const ulong TAG = 'Trce';
const ulong USECS_IN_SEC = consts::NANO_SECS_IN_SEC / consts::NANO_SECS_IN_USEC;

/* ---- the events ---- */

static_assert(sizeof(Test) <= TraceRing::PAYLOAD_BYTES, "a payload fits a slot");

const Field TEST_FIELDS[] = {
    { "seq", __builtin_offsetof(Test, seq), sizeof(u64), Kind::Unsigned, nullptr, 0 },
    { "value", __builtin_offsetof(Test, value), sizeof(u64), Kind::Hex, nullptr, 0 },
};

/* ---- the scheduler's events ----
   A payload is 40 bytes: a task said by its pid and up to 12 bytes of its
   name is 16, so a switch -- two of them, a CPU's id and two flags -- is
   38, padded to the slot's 40; a CPU or a slice is one word of 4. A name is
   written once, when the task is made, and never moved: what a record holds
   of one is what it was called. */

static_assert(sizeof(SchedSwitch) <= TraceRing::PAYLOAD_BYTES, "a payload fits a slot");
static_assert(sizeof(SchedWakeup) <= TraceRing::PAYLOAD_BYTES, "a payload fits a slot");
static_assert(sizeof(SchedBlock) <= TraceRing::PAYLOAD_BYTES, "a payload fits a slot");
static_assert(sizeof(SchedMigrate) <= TraceRing::PAYLOAD_BYTES, "a payload fits a slot");
static_assert(sizeof(SchedNew) <= TraceRing::PAYLOAD_BYTES, "a payload fits a slot");
static_assert(sizeof(SchedExit) <= TraceRing::PAYLOAD_BYTES, "a payload fits a slot");
static_assert(sizeof(SchedIpi) <= TraceRing::PAYLOAD_BYTES, "a payload fits a slot");
static_assert(sizeof(SchedPreemptDeferred) <= TraceRing::PAYLOAD_BYTES, "a payload fits a slot");
static_assert(sizeof(Idle) <= TraceRing::PAYLOAD_BYTES, "a payload fits a slot");
static_assert(sizeof(SchedSlice) <= TraceRing::PAYLOAD_BYTES, "a payload fits a slot");

const char* const SCHED_REASONS[] = { "voluntary", "yield", "preempt" };
const char* const SWITCH_STATES[] = { "runnable", "blocked", "sleeping", "exited" };

const Field SCHED_SWITCH_FIELDS[] = {
    { "prev_pid", __builtin_offsetof(SchedSwitch, prev_pid), sizeof(u32), Kind::Pid, nullptr, 0 },
    { "prev_comm", __builtin_offsetof(SchedSwitch, prev_comm), COMM_BYTES, Kind::Comm, nullptr, 0 },
    { "next_pid", __builtin_offsetof(SchedSwitch, next_pid), sizeof(u32), Kind::Pid, nullptr, 0 },
    { "next_comm", __builtin_offsetof(SchedSwitch, next_comm), COMM_BYTES, Kind::Comm, nullptr, 0 },
    { "next_cpu", __builtin_offsetof(SchedSwitch, next_cpu), sizeof(u32), Kind::Cpu, nullptr, 0 },
    { "prev_state", __builtin_offsetof(SchedSwitch, prev_state), sizeof(u8), Kind::Enum,
      SWITCH_STATES, sizeof(SWITCH_STATES) / sizeof(SWITCH_STATES[0]) },
    { "reason", __builtin_offsetof(SchedSwitch, reason), sizeof(u8), Kind::Enum,
      SCHED_REASONS, sizeof(SCHED_REASONS) / sizeof(SCHED_REASONS[0]) },
};

const Field SCHED_WAKEUP_FIELDS[] = {
    { "pid", __builtin_offsetof(SchedWakeup, pid), sizeof(u32), Kind::Pid, nullptr, 0 },
    { "comm", __builtin_offsetof(SchedWakeup, comm), COMM_BYTES, Kind::Comm, nullptr, 0 },
    { "cpu", __builtin_offsetof(SchedWakeup, cpu), sizeof(u32), Kind::Cpu, nullptr, 0 },
    /* 1: its deadline came; 0: someone's unblock() */
    { "deadline", __builtin_offsetof(SchedWakeup, deadline), sizeof(u8), Kind::Unsigned, nullptr, 0 },
    /* Who woke it: the task the waker ran on -- the one an interrupt landed
       on, for a wakeup from an interrupt's context */
    { "waker_pid", __builtin_offsetof(SchedWakeup, waker_pid), sizeof(u32), Kind::Pid, nullptr, 0 },
    /* A deadline's: how late the sleep list was walked past it -- the wait
       is counted from the deadline */
    { "late_ns", __builtin_offsetof(SchedWakeup, late_ns), sizeof(u32), Kind::Ns, nullptr, 0 },
};

const Field SCHED_BLOCK_FIELDS[] = {
    { "pid", __builtin_offsetof(SchedBlock, pid), sizeof(u32), Kind::Pid, nullptr, 0 },
    { "comm", __builtin_offsetof(SchedBlock, comm), COMM_BYTES, Kind::Comm, nullptr, 0 },
    { "cpu", __builtin_offsetof(SchedBlock, cpu), sizeof(u32), Kind::Cpu, nullptr, 0 },
    /* 1: asleep until a deadline (sleep, Event::wait_for); 0: waiting on
       someone else's wakeup */
    { "deadline", __builtin_offsetof(SchedBlock, deadline), sizeof(u8), Kind::Unsigned, nullptr, 0 },
    /* How long it was on a CPU this time */
    { "ran_ns", __builtin_offsetof(SchedBlock, ran_ns), sizeof(u32), Kind::Ns, nullptr, 0 },
};

const Field SCHED_MIGRATE_FIELDS[] = {
    { "pid", __builtin_offsetof(SchedMigrate, pid), sizeof(u32), Kind::Pid, nullptr, 0 },
    { "comm", __builtin_offsetof(SchedMigrate, comm), COMM_BYTES, Kind::Comm, nullptr, 0 },
    { "from", __builtin_offsetof(SchedMigrate, from), sizeof(u32), Kind::Cpu, nullptr, 0 },
    { "to", __builtin_offsetof(SchedMigrate, to), sizeof(u32), Kind::Cpu, nullptr, 0 },
};

const Field SCHED_NEW_FIELDS[] = {
    { "pid", __builtin_offsetof(SchedNew, pid), sizeof(u32), Kind::Pid, nullptr, 0 },
    { "comm", __builtin_offsetof(SchedNew, comm), COMM_BYTES, Kind::Comm, nullptr, 0 },
    { "cpu", __builtin_offsetof(SchedNew, cpu), sizeof(u32), Kind::Cpu, nullptr, 0 },
};

const Field SCHED_EXIT_FIELDS[] = {
    { "pid", __builtin_offsetof(SchedExit, pid), sizeof(u32), Kind::Pid, nullptr, 0 },
    { "comm", __builtin_offsetof(SchedExit, comm), COMM_BYTES, Kind::Comm, nullptr, 0 },
    { "cpu", __builtin_offsetof(SchedExit, cpu), sizeof(u32), Kind::Cpu, nullptr, 0 },
};

const Field SCHED_IPI_FIELDS[] = {
    { "cpu", __builtin_offsetof(SchedIpi, cpu), sizeof(u32), Kind::Cpu, nullptr, 0 },
    /* What queued work the interrupt found to run (process_ipi_tasks) */
    { "tasks", __builtin_offsetof(SchedIpi, tasks), sizeof(u32), Kind::Unsigned, nullptr, 0 },
};

const Field SCHED_PREEMPT_DEFERRED_FIELDS[] = {
    { "pid", __builtin_offsetof(SchedPreemptDeferred, pid), sizeof(u32), Kind::Pid, nullptr, 0 },
    { "comm", __builtin_offsetof(SchedPreemptDeferred, comm), COMM_BYTES, Kind::Comm, nullptr, 0 },
    { "cpu", __builtin_offsetof(SchedPreemptDeferred, cpu), sizeof(u32), Kind::Cpu, nullptr, 0 },
    /* Its preempt_disable_counter_ as the reschedule found it: a lock, most
       likely, whose holder was still in it */
    { "count", __builtin_offsetof(SchedPreemptDeferred, count), sizeof(u32), Kind::Unsigned, nullptr, 0 },
    { "idle", __builtin_offsetof(SchedPreemptDeferred, idle), sizeof(u8), Kind::Unsigned, nullptr, 0 },
};

const Field IDLE_FIELDS[] = {
    { "cpu", __builtin_offsetof(Idle, cpu), sizeof(u32), Kind::Cpu, nullptr, 0 },
    { "pid", __builtin_offsetof(Idle, pid), sizeof(u32), Kind::Pid, nullptr, 0 },
};

const Field SCHED_SLICE_FIELDS[] = {
    { "cpu", __builtin_offsetof(SchedSlice, cpu), sizeof(u32), Kind::Cpu, nullptr, 0 },
    { "pid", __builtin_offsetof(SchedSlice, pid), sizeof(u32), Kind::Pid, nullptr, 0 },
    /* How long from now the current one's slice ends */
    { "slice_ns", __builtin_offsetof(SchedSlice, slice_ns), sizeof(u32), Kind::Ns, nullptr, 0 },
    /* The tasks the queue had that could run */
    { "runnable", __builtin_offsetof(SchedSlice, runnable), sizeof(u32), Kind::Unsigned, nullptr, 0 },
};

/* ---- the hypervisor's events ----
   Their sites are the hv module's Rust, whose payloads (kcore/src/trace/
   events.rs) every boot holds to these, an enumeration's names included:
   a name changed here alone, or there alone, is a failed boot. Each field's
   order here is the order it prints in; its offset is the payload's own. A
   time of 4.29 s or more is held at that, the most a u32 of ns has. */

static_assert(sizeof(HvExit) <= TraceRing::PAYLOAD_BYTES, "a payload fits a slot");
static_assert(sizeof(HvInject) <= TraceRing::PAYLOAD_BYTES, "a payload fits a slot");
static_assert(sizeof(HvHalt) <= TraceRing::PAYLOAD_BYTES, "a payload fits a slot");
static_assert(sizeof(HvWake) <= TraceRing::PAYLOAD_BYTES, "a payload fits a slot");
static_assert(sizeof(HvTimer) <= TraceRing::PAYLOAD_BYTES, "a payload fits a slot");
static_assert(sizeof(HvRing) <= TraceRing::PAYLOAD_BYTES, "a payload fits a slot");

/* hv's `Exit`, a value a kind (src/crates/hv/src/svm.rs) */
const char* const HV_EXIT_REASONS[] = { "host", "kicked", "io", "hlt", "cpuid", "msr", "hypercall", "irq_window",
    "nmi_window", "npf", "exception", "machine_check", "shutdown", "invalid", "cr8", "cr0", "cr4", "dr", "eoi",
    "pause", "undefined", "other" };
const char* const HV_INJECT_KINDS[] = { "exception", "nmi", "pic", "apic", "window" };
const char* const HV_HALT_UNTIL[] = { "timer", "pit", "device", "deadline", "nothing" };
const char* const HV_WAKE_HOW[] = { "polled", "rung", "timed", "advanced" };
/* The run loop's `Step` before the round that found the edge: what made the
   interrupt late, when it was */
const char* const HV_TIMER_HOW[] = { "nothing", "woke_late", "woke_on_time", "not_slept_for", "spun", "off_cpu",
    "host", "exit", "other" };
const char* const HV_RING_HOW[] = { "rung", "polling", "notified", "signalled" };

const Field HV_EXIT_FIELDS[] = {
    /* The guest's CPU, by its number in its guest */
    { "vcpu", __builtin_offsetof(HvExit, vcpu), sizeof(u16), Kind::Unsigned, nullptr, 0 },
    /* Its task, which tells one VM's CPU 0 from another's */
    { "pid", __builtin_offsetof(HvExit, pid), sizeof(u32), Kind::Pid, nullptr, 0 },
    { "reason", __builtin_offsetof(HvExit, reason), sizeof(u8), Kind::Enum,
      HV_EXIT_REASONS, sizeof(HV_EXIT_REASONS) / sizeof(HV_EXIT_REASONS[0]) },
    /* What it was about, by its reason: io a port, msr an MSR, npf a guest
       physical address, exception and eoi a vector, cpuid a leaf,
       hypercall RAX, cr0 the value, cr4 and cr8 the general-purpose
       register's number (0xFF where the instruction is decoded for it), dr
       the debug register's; host the host's vector that took it out --
       VT-x's, 0 under AMD-V, which does not say; other the CPU's own exit
       code */
    { "detail", __builtin_offsetof(HvExit, detail), sizeof(u64), Kind::Hex, nullptr, 0 },
    /* And more, by its reason: io its size, 0x100 an IN, 0x200 a string,
       0x400 REP; msr, cr0, cr4, cr8 and dr 1 for a write; cpuid the
       subleaf; npf and exception the error code */
    { "info", __builtin_offsetof(HvExit, info), sizeof(u32), Kind::Hex, nullptr, 0 },
    /* In the guest: from its entry -- vmrun, vmlaunch, vmresume -- to its
       exit, as the cycle counter has them; 0 for one kicked, never entered */
    { "guest_ns", __builtin_offsetof(HvExit, guest_ns), sizeof(u32), Kind::Ns, nullptr, 0 },
    /* Out of the guest before that entry: from the exit before it -- the
       host's handling of it, the loop's look, a halt's sleep; 0 when that
       one was not timed, the event having been off */
    { "out_ns", __builtin_offsetof(HvExit, out_ns), sizeof(u32), Kind::Ns, nullptr, 0 },
    /* From the exit to this record: the host's interrupt it was for, taken
       first, and anything that ran in its wake */
    { "since_ns", __builtin_offsetof(HvExit, since_ns), sizeof(u32), Kind::Ns, nullptr, 0 },
    { "rip", __builtin_offsetof(HvExit, rip), sizeof(u64), Kind::Hex, nullptr, 0 },
};

const Field HV_INJECT_FIELDS[] = {
    { "vcpu", __builtin_offsetof(HvInject, vcpu), sizeof(u16), Kind::Unsigned, nullptr, 0 },
    { "pid", __builtin_offsetof(HvInject, pid), sizeof(u32), Kind::Pid, nullptr, 0 },
    /* exception one of its own given back, nmi, pic the 8259's through
       LINT0, apic its local APIC's; window none can go in, and the CPU is
       to exit when one can */
    { "kind", __builtin_offsetof(HvInject, kind), sizeof(u8), Kind::Enum,
      HV_INJECT_KINDS, sizeof(HV_INJECT_KINDS) / sizeof(HV_INJECT_KINDS[0]) },
    { "vector", __builtin_offsetof(HvInject, vector), sizeof(u8), Kind::Hex, nullptr, 0 },
};

const Field HV_HALT_FIELDS[] = {
    { "vcpu", __builtin_offsetof(HvHalt, vcpu), sizeof(u16), Kind::Unsigned, nullptr, 0 },
    { "pid", __builtin_offsetof(HvHalt, pid), sizeof(u32), Kind::Pid, nullptr, 0 },
    /* What its sleep ends at: its APIC timer's edge, the PIT's, the
       devices' next round, the end of its run, or nothing armed -- the
       longest it waits without a look */
    { "until", __builtin_offsetof(HvHalt, until), sizeof(u8), Kind::Enum,
      HV_HALT_UNTIL, sizeof(HV_HALT_UNTIL) / sizeof(HV_HALT_UNTIL[0]) },
    { "wait_ns", __builtin_offsetof(HvHalt, wait_ns), sizeof(u32), Kind::Ns, nullptr, 0 },
    /* The spin for a ring it tries first: 0 for none */
    { "poll_ns", __builtin_offsetof(HvHalt, poll_ns), sizeof(u32), Kind::Ns, nullptr, 0 },
};

const Field HV_WAKE_FIELDS[] = {
    { "vcpu", __builtin_offsetof(HvWake, vcpu), sizeof(u16), Kind::Unsigned, nullptr, 0 },
    { "pid", __builtin_offsetof(HvWake, pid), sizeof(u32), Kind::Pid, nullptr, 0 },
    /* polled: rung within the spin; rung: while asleep; timed: its time
       came; advanced: woke short of its timer's edge and spun the rest */
    { "how", __builtin_offsetof(HvWake, how), sizeof(u8), Kind::Enum,
      HV_WAKE_HOW, sizeof(HV_WAKE_HOW) / sizeof(HV_WAKE_HOW[0]) },
    /* The halt, from its look to now: the spin, the sleep and the wake */
    { "halted_ns", __builtin_offsetof(HvWake, halted_ns), sizeof(u32), Kind::Ns, nullptr, 0 },
    /* How long past the time it slept to it woke: the host's wake of it */
    { "late_ns", __builtin_offsetof(HvWake, late_ns), sizeof(u32), Kind::Ns, nullptr, 0 },
};

const Field HV_TIMER_FIELDS[] = {
    { "vcpu", __builtin_offsetof(HvTimer, vcpu), sizeof(u16), Kind::Unsigned, nullptr, 0 },
    { "pid", __builtin_offsetof(HvTimer, pid), sizeof(u32), Kind::Pid, nullptr, 0 },
    /* What its task did before the round that found the edge */
    { "how", __builtin_offsetof(HvTimer, how), sizeof(u8), Kind::Enum,
      HV_TIMER_HOW, sizeof(HV_TIMER_HOW) / sizeof(HV_TIMER_HOW[0]) },
    /* How long past its edge its interrupt was asked for */
    { "late_ns", __builtin_offsetof(HvTimer, late_ns), sizeof(u32), Kind::Ns, nullptr, 0 },
};

const Field HV_RING_FIELDS[] = {
    /* The guest CPU rung */
    { "vcpu", __builtin_offsetof(HvRing, vcpu), sizeof(u16), Kind::Unsigned, nullptr, 0 },
    /* The task that rang it: another of its guest's CPUs', a device's, or
       the one an interrupt landed on */
    { "pid", __builtin_offsetof(HvRing, pid), sizeof(u32), Kind::Pid, nullptr, 0 },
    /* rung: its event signalled and its kick sent; polling: found spinning
       for one, the word and the kick alone; notified: a posted interrupt,
       the CPU notified to take it in the guest; signalled: its event alone,
       for what waits for its next exit */
    { "how", __builtin_offsetof(HvRing, how), sizeof(u8), Kind::Enum,
      HV_RING_HOW, sizeof(HV_RING_HOW) / sizeof(HV_RING_HOW[0]) },
};

/* Sized by its lines, not by COUNT: an event added to tp::Id without its line
   here would be a description of zeroes -- a null name printed -- where now it
   is a build that fails. */
const Event EVENTS[] = {
    { "test", "test", "what `trace test` records: a sequence number and a value",
      sizeof(Test), sizeof(TEST_FIELDS) / sizeof(TEST_FIELDS[0]), TEST_FIELDS },
    { "sched_switch", "sched", "a CPU's next task, and the one it left behind: the decision's outcome, its reason and "
      "the state the one before went with",
      sizeof(SchedSwitch), sizeof(SCHED_SWITCH_FIELDS) / sizeof(SCHED_SWITCH_FIELDS[0]), SCHED_SWITCH_FIELDS },
    { "sched_wakeup", "sched", "a task asleep handed back to its queue by an unblock() or by its deadline",
      sizeof(SchedWakeup), sizeof(SCHED_WAKEUP_FIELDS) / sizeof(SCHED_WAKEUP_FIELDS[0]), SCHED_WAKEUP_FIELDS },
    { "sched_block", "sched", "a task that left its CPU because it cannot run: asleep until a deadline, or waiting "
      "on someone else's wakeup",
      sizeof(SchedBlock), sizeof(SCHED_BLOCK_FIELDS) / sizeof(SCHED_BLOCK_FIELDS[0]), SCHED_BLOCK_FIELDS },
    { "sched_migrate", "sched", "a task that could still run, moved by the load balancer from one CPU's queue to "
      "another's",
      sizeof(SchedMigrate), sizeof(SCHED_MIGRATE_FIELDS) / sizeof(SCHED_MIGRATE_FIELDS[0]), SCHED_MIGRATE_FIELDS },
    { "sched_new", "sched", "a task put on a CPU's queue for the first time it runs",
      sizeof(SchedNew), sizeof(SCHED_NEW_FIELDS) / sizeof(SCHED_NEW_FIELDS[0]), SCHED_NEW_FIELDS },
    { "sched_exit", "sched", "a task that has exited, as it gives its pid back: before any later task can be "
      "given that pid",
      sizeof(SchedExit), sizeof(SCHED_EXIT_FIELDS) / sizeof(SCHED_EXIT_FIELDS[0]), SCHED_EXIT_FIELDS },
    { "sched_ipi", "sched", "an interrupt another CPU sent, and the queued work it found: a wakeup's reschedule, a "
      "TLB shootdown",
      sizeof(SchedIpi), sizeof(SCHED_IPI_FIELDS) / sizeof(SCHED_IPI_FIELDS[0]), SCHED_IPI_FIELDS },
    { "sched_preempt_deferred", "sched", "a reschedule that found the task it landed on with preemption disabled, "
      "and put it off until the count was back to zero",
      sizeof(SchedPreemptDeferred), sizeof(SCHED_PREEMPT_DEFERRED_FIELDS) / sizeof(SCHED_PREEMPT_DEFERRED_FIELDS[0]),
      SCHED_PREEMPT_DEFERRED_FIELDS },
    { "idle_enter", "idle", "a CPU with nothing to run, about to halt",
      sizeof(Idle), sizeof(IDLE_FIELDS) / sizeof(IDLE_FIELDS[0]), IDLE_FIELDS },
    { "idle_exit", "idle", "that CPU's idle over: the halt returned, or a decision took the CPU from the idle "
      "task first",
      sizeof(Idle), sizeof(IDLE_FIELDS) / sizeof(IDLE_FIELDS[0]), IDLE_FIELDS },
    { "sched_slice", "debug", "the slice the current task was given, from the decision that armed this CPU's timer "
      "for its deadline",
      sizeof(SchedSlice), sizeof(SCHED_SLICE_FIELDS) / sizeof(SCHED_SLICE_FIELDS[0]), SCHED_SLICE_FIELDS },
    { "hv_exit", "hv", "a guest CPU out of its guest, or turned back on its way in: why, what about, how long it ran "
      "in the guest and was out before",
      sizeof(HvExit), sizeof(HV_EXIT_FIELDS) / sizeof(HV_EXIT_FIELDS[0]), HV_EXIT_FIELDS },
    { "hv_inject", "hv", "what goes into a guest CPU at its entry: an exception given back, an NMI, an interrupt "
      "-- or none it can take yet",
      sizeof(HvInject), sizeof(HV_INJECT_FIELDS) / sizeof(HV_INJECT_FIELDS[0]), HV_INJECT_FIELDS },
    { "hv_halt", "hv", "a guest CPU halted with nothing to take, its task about to spin and sleep until what it "
      "waits for",
      sizeof(HvHalt), sizeof(HV_HALT_FIELDS) / sizeof(HV_HALT_FIELDS[0]), HV_HALT_FIELDS },
    { "hv_wake", "hv", "that halt over: rung, or its time come, and how late past it",
      sizeof(HvWake), sizeof(HV_WAKE_FIELDS) / sizeof(HV_WAKE_FIELDS[0]), HV_WAKE_FIELDS },
    { "hv_timer", "hv", "a guest CPU's APIC timer interrupt asked for, and how late past its edge",
      sizeof(HvTimer), sizeof(HV_TIMER_FIELDS) / sizeof(HV_TIMER_FIELDS[0]), HV_TIMER_FIELDS },
    { "hv_ring", "hv", "a guest CPU rung, or notified, by whoever has something for it: another CPU's IPI, a "
      "device, a stop",
      sizeof(HvRing), sizeof(HV_RING_FIELDS) / sizeof(HV_RING_FIELDS[0]), HV_RING_FIELDS },
};
static_assert(sizeof(EVENTS) / sizeof(EVENTS[0]) == COUNT, "every event has its description, in tp::Id's order");


/* ---- the rings ---- */

/* Each running CPU's ring, made by the first `trace on` and swapped for new
   ones by `trace size` (null by `trace free`): read with acquire by emit(),
   which writes into whichever it finds. One swapped out is freed only once
   every other CPU has taken an interrupt since (CpuTable::synchronize):
   emit() writes with interrupts off, so none is still inside it then. */
TraceRing* rings[MAX_CPUS] = {};

/* Set by `trace freeze`: every emit() takes nothing, so that what is in the
   rings stays as it was for reading. */
u8 frozen = 0;

/* The one the trigger fired on, and what it was: a seqlock, as a ring's slot
   is -- written once, by the CPU that claimed it (`claim`), read by
   `trace stat`, which may race it. frozen follows the whole record, so that
   whoever finds the rings frozen can read what froze them. A wakeup that
   claimed the trigger a moment before it was re-armed can still freeze the
   new arming; `trace stat` says which wait it was. */
struct WakeupTrigger
{
    /* Odd while the record is being written, and 0 before any firing */
    u32 seq;
    /* The claim: the first CPU to exchange this to 1 writes the record */
    u32 claim;
    u64 waited_ns;
    u64 at_ns;
    /* The task's pid in the high half, the cpu it waited on in the low */
    u64 pid_cpu;
    /* COMM_BYTES of the task's name, zero-padded, as two words */
    u64 comm[2];
};

WakeupTrigger trigger = {};

/* The trigger off, and every firing of what was armed before done with:
   note_wakeup runs with interrupts off (TaskQueue::note_run, inside the
   decision), so a CPU that has taken an interrupt since the threshold went
   to 0 is out of any note_wakeup that read the old one -- its record
   written, its freeze and its threshold of 0 stored -- and none can land
   after what the caller does next. With `forget`, what it fired on is
   forgotten as well. Under control(); task context. */
void disarm_trigger(bool forget)
{
    __atomic_store_n(&wakeup_trigger_ns, 0, __ATOMIC_RELEASE);
    hal::smp_mb();
    CpuTable::get_instance().synchronize("trace trigger");
    if (forget)
    {
        __atomic_store_n(&trigger.claim, 0, __ATOMIC_RELAXED);
        __atomic_store_n(&trigger.seq, 0, __ATOMIC_RELEASE);
    }
}

/* Armed at `over_ns`, recording: whatever it fired on before forgotten,
   and the rings thawed -- a frozen ring has nothing for it to keep. Under
   control(). */
void arm_trigger(u64 over_ns)
{
    disarm_trigger(true);
    __atomic_store_n(&frozen, 0, __ATOMIC_RELEASE);
    __atomic_store_n(&wakeup_trigger_ns, over_ns, __ATOMIC_RELEASE);
}

/* What the trigger fired on, if it has: a seqlock read of the record, as a
   ring slot's is. False before any firing; false and `busy` when it was
   being written at every look -- a writer has interrupts off for a few
   stores, so that is a report, not a wait. */
bool triggered(WakeupTrigger& out, bool& busy)
{
    const ulong LOOKS = 1000;
    busy = false;
    for (ulong look = 0; look < LOOKS; look++)
    {
        u32 seq = __atomic_load_n(&trigger.seq, __ATOMIC_ACQUIRE);
        if (seq == 0)
            return false;
        if ((seq & 1) != 0)
        {
            cpu_relax();
            continue;
        }
        out.seq = seq;
        out.claim = 1;
        out.waited_ns = __atomic_load_n(&trigger.waited_ns, __ATOMIC_RELAXED);
        out.at_ns = __atomic_load_n(&trigger.at_ns, __ATOMIC_RELAXED);
        out.pid_cpu = __atomic_load_n(&trigger.pid_cpu, __ATOMIC_RELAXED);
        out.comm[0] = __atomic_load_n(&trigger.comm[0], __ATOMIC_RELAXED);
        out.comm[1] = __atomic_load_n(&trigger.comm[1], __ATOMIC_RELAXED);
        hal::smp_rmb();
        if (__atomic_load_n(&trigger.seq, __ATOMIC_ACQUIRE) == seq)
            return true;
    }
    busy = true;
    return false;
}

/* Back to recording, with no trigger and nothing it fired on: what
   `trace thaw` does, and `trace free` and `trace size`, whose rings are
   new. Under control(). */
void thaw()
{
    disarm_trigger(true);
    __atomic_store_n(&frozen, 0, __ATOMIC_RELEASE);
}

/* The size `trace on` makes each CPU's ring at, in KiB: a chunk's (64 KiB)
   times a power of two. 1 MiB: 16384 records a CPU. */
const ulong DEFAULT_RING_KIB = 1024;
const ulong MIN_RING_KIB = TraceRing::CHUNK_BYTES / consts::KB;
const ulong MAX_RING_KIB = TraceRing::MAX_CHUNKS * MIN_RING_KIB;
/* Read and written as an atomic: ring_size_kib() reads it without the lock. */
ulong ring_kib = DEFAULT_RING_KIB;

/* Per ring, the index `trace show` reads from: what `trace clear` left
   behind it. Reset to 0 when its ring is new. */
u64 cleared_at[MAX_CPUS] = {};

/* What `trace show` prints at most, and by default. */
const ulong SHOW_MAX = 4096;
const ulong SHOW_DEFAULT = 50;

/* Every command's: making and freeing rings, reading them, clearing. Task
   context only; emit() never takes it. A function's static: the link refuses
   one with a constructor at namespace scope. */
Mutex& control()
{
    static Mutex m;
    return m;
}

bool valid_ring_kib(ulong kib)
{
    if (kib < MIN_RING_KIB || kib > MAX_RING_KIB || kib % MIN_RING_KIB != 0)
        return false;
    ulong chunks = kib / MIN_RING_KIB;
    return (chunks & (chunks - 1)) == 0;
}

/* Every ring swapped for `fresh[i]` (null for none), the old ones freed once
   no CPU can still be writing into them. Under control(). */
void swap_rings(TraceRing* fresh[MAX_CPUS])
{
    TraceRing* old[MAX_CPUS];
    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        old[i] = __atomic_exchange_n(&rings[i], fresh[i], __ATOMIC_ACQ_REL);
        cleared_at[i] = 0;
    }
    /* The new pointers seen by every CPU before the interrupt that tells
       this one it is out of any write into an old ring. */
    hal::smp_mb();
    CpuTable::get_instance().synchronize("tracepoint rings");

    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        if (old[i] == nullptr)
            continue;
        old[i]->release();
        delete old[i];
    }
}

/* A ring of ring_kib for every running CPU, in place of what there was.
   Under control(). False, with the rings as they were, for want of memory. */
bool make_rings(stdlib::Printer& con)
{
    ulong chunks = ring_size_kib() / MIN_RING_KIB;
    ulong running = CpuTable::get_instance().get_running_cpus();
    TraceRing* fresh[MAX_CPUS] = {};

    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        if (!(running & (1UL << i)))
            continue;
        fresh[i] = new (mm::NO_THROW) TraceRing();
        if (fresh[i] == nullptr || !fresh[i]->init(chunks, TAG))
        {
            con.printf("trace: no memory for a ring of %lu KiB for cpu %lu\n", ring_size_kib(), i);
            /* Nothing kept of a part, in the reverse order. */
            for (ulong j = i + 1; j > 0; j--)
            {
                if (fresh[j - 1] == nullptr)
                    continue;
                fresh[j - 1]->release();
                delete fresh[j - 1];
            }
            return false;
        }
    }

    swap_rings(fresh);
    return true;
}

bool have_rings()
{
    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        if (__atomic_load_n(&rings[i], __ATOMIC_ACQUIRE) != nullptr)
            return true;
    }
    return false;
}

/* ---- reading ---- */

/* Nanoseconds since boot of a record taken at `cycles`, given the counter
   and the clock read together at `now_cycles` and `now_ns`. */
ulong record_ns(u64 cycles, u64 now_cycles, ulong now_ns)
{
    if (cycles <= now_cycles)
    {
        ulong back = cycle_counter_delta_to_ns(now_cycles - cycles);
        return (back < now_ns) ? now_ns - back : 0;
    }
    return now_ns + cycle_counter_delta_to_ns(cycles - now_cycles);
}

/* A field's value out of a record's payload: its bytes, widened -- a signed
   one with its sign. */
u64 field_value(const TraceRing::Record& r, const Field& f)
{
    u64 v = 0;
    if (f.size == 0 || f.size > sizeof(v) || f.offset + f.size > sizeof(r.payload))
        return 0;
    stdlib::mem_cpy(&v, reinterpret_cast<const u8*>(r.payload) + f.offset, f.size);
    if (f.kind == Kind::Signed && f.size < sizeof(v) && (v >> (8 * f.size - 1)) & 1)
        v |= ~0ULL << (8 * f.size);
    return v;
}

/* A Comm field's bytes, up to its first zero, as far as the field holds and
   `len` allows, terminated: what a record holds of a name. A record is
   copied out of the ring whole, so whatever follows the zero in the payload
   is never read for one. */
void read_comm(const TraceRing::Record& r, const Field& f, char* to, ulong len)
{
    if (len == 0)
        return;
    ulong room = (f.size < COMM_BYTES) ? f.size : COMM_BYTES;
    if (room > len - 1)
        room = len - 1;
    if (f.offset + room > sizeof(r.payload))
        room = 0;
    stdlib::mem_set(to, 0, len);
    if (room != 0)
        stdlib::mem_cpy(to, reinterpret_cast<const u8*>(r.payload) + f.offset, room);
    to[room] = '\0';
}

void print_record(stdlib::Printer& con, const TraceRing::Record& r, ulong ns)
{
    con.printf("%5lu.%06lu %2u ", ns / consts::NANO_SECS_IN_SEC,
        (ns / consts::NANO_SECS_IN_USEC) % USECS_IN_SEC, r.cpu);
    if (r.id >= COUNT || r.size != EVENTS[r.id].size)
    {
        con.printf("event %u of %u bytes, which this kernel does not know\n", (unsigned)r.id, (unsigned)r.size);
        return;
    }

    const Event& e = EVENTS[r.id];
    con.printf("%s:", e.name);
    for (ulong i = 0; i < e.field_count; i++)
    {
        const Field& f = e.fields[i];
        u64 v = field_value(r, f);
        switch (f.kind)
        {
        case Kind::Signed:
            con.printf(" %s=%ld", f.name, (long)v);
            break;
        case Kind::Hex:
            con.printf(" %s=0x%lX", f.name, (ulong)v);
            break;
        case Kind::Ns:
            con.printf(" %s=%lu.%03luus", f.name, (ulong)v / consts::NANO_SECS_IN_USEC,
                (ulong)v % consts::NANO_SECS_IN_USEC);
            break;
        case Kind::Enum:
            if (v < f.name_count)
                con.printf(" %s=%s", f.name, f.names[v]);
            else
                con.printf(" %s=%lu", f.name, (ulong)v);
            break;
        case Kind::Comm:
        {
            char name[COMM_BYTES + 1];
            read_comm(r, f, name, sizeof(name));
            con.printf(" %s=%s", f.name, name);
            break;
        }
        case Kind::Unsigned:
        case Kind::Cpu:
        case Kind::Pid:
        default:
            con.printf(" %s=%lu", f.name, (ulong)v);
            break;
        }
    }
    con.printf("\n");
}

/* Whether `word` names events -- one's name, a group's, or `all` -- with
   their bits put into `mask`. */
bool name_mask(const char* word, EventMask& mask)
{
    bool found = false;
    for (ulong i = 0; i < COUNT; i++)
    {
        if (stdlib::str_cmp(word, "all") == 0 || stdlib::str_cmp(word, EVENTS[i].name) == 0
            || stdlib::str_cmp(word, EVENTS[i].group) == 0)
        {
            mask |= event_bit(i);
            found = true;
        }
    }
    return found;
}

/* The window show, dump and latency read: the newest `count` records of the
   events in `mask` of every ring -- or of cpu `only`'s -- a Window a CPU,
   each oldest first (read_window): how many there are across them. Under
   control(); what it holds is the caller's, given back by release_windows.
   A CPU whose part will not fit in memory is said and left out, its part
   marked COMPLETE_NEVER: no wait is paired across a CPU nobody read. */
ulong collect(Window windows[MAX_CPUS], stdlib::Printer& con, ulong count, ulong only, EventMask mask)
{
    ulong total = 0;
    for (ulong cpu = 0; cpu < MAX_CPUS; cpu++)
    {
        windows[cpu] = {};
        TraceRing* ring = __atomic_load_n(&rings[cpu], __ATOMIC_ACQUIRE);
        if (ring == nullptr || (only != MAX_CPUS && cpu != only))
            continue;

        /* Room for `count`, but no more than the ring holds or was written
           since the clear */
        u64 head = ring->head();
        if (head <= cleared_at[cpu])
            continue;
        ulong want = count;
        if (want > ring->capacity())
            want = ring->capacity();
        if (head - cleared_at[cpu] < want)
            want = (ulong)(head - cleared_at[cpu]);
        if (want == 0)
            continue;

        auto records = static_cast<TraceRing::Record*>(mm::alloc(want * sizeof(TraceRing::Record), TAG));
        if (records == nullptr)
        {
            con.printf("trace: no memory to read cpu %lu's ring: left out\n", cpu);
            windows[cpu].complete_from = COMPLETE_NEVER;
            continue;
        }
        u64 complete;
        ulong kept = read_window(*ring, cleared_at[cpu], want, mask, (u32)cpu, records, complete);
        windows[cpu] = { records, kept, 0, complete };
        total += kept;
    }
    return total;
}

/* What collect() took, given back. */
void release_windows(Window windows[MAX_CPUS])
{
    for (ulong cpu = 0; cpu < MAX_CPUS; cpu++)
    {
        if (windows[cpu].records != nullptr)
            mm::free(windows[cpu].records);
        windows[cpu] = {};
    }
}

/* The clock and the counter together, once: every record's time is counted
   back from them. */
void now_point(u64& now_cycles, ulong& now_ns)
{
    ulong flags = hal::irq_save();
    now_cycles = hal::read_cycle_counter();
    now_ns = get_boot_time().get_value();
    hal::irq_restore(flags);
}

/* Why there is nothing to show: no ring, or the rings frozen, or nothing in
   them. */
void nothing_recorded(stdlib::Printer& con)
{
    if (!have_rings())
        con.printf("trace: nothing recorded -- no event has been turned on\n");
    else if (__atomic_load_n(&frozen, __ATOMIC_RELAXED))
        con.printf("trace: nothing recorded -- the rings are frozen: `trace thaw` to record\n");
    else
        con.printf("trace: nothing recorded\n");
}

/* The cycle count of the window's first record past the `skip` oldest: where
   what is shown begins. The windows' places are put back. */
u64 first_shown(Window windows[MAX_CPUS], ulong skip)
{
    const TraceRing::Record* r = nullptr;
    u64 first = 0;
    for (ulong taken = 0; merge_next(windows, MAX_CPUS, r); taken++)
    {
        if (taken == skip)
        {
            first = r->cycles;
            break;
        }
    }
    for (ulong cpu = 0; cpu < MAX_CPUS; cpu++)
        windows[cpu].next = 0;
    return first;
}

/* A CPU's part of what is shown that begins later than the rest -- its older
   records written over, or left out for room -- said, so that its silence
   before then is not read as a CPU that did nothing. */
void say_cut(stdlib::Printer& con, Window windows[MAX_CPUS], ulong skip, u64 now_cycles, ulong now_ns)
{
    u64 first = first_shown(windows, skip);
    for (ulong cpu = 0; cpu < MAX_CPUS; cpu++)
    {
        u64 from = windows[cpu].complete_from;
        if (from == COMPLETE_NEVER)
        {
            con.printf("trace: cpu %lu: nothing of it could be read\n", cpu);
            continue;
        }
        if (from == 0 || from <= first)
            continue;
        ulong ns = record_ns(from, now_cycles, now_ns);
        con.printf("trace: cpu %lu: nothing before %lu.%06lu -- its older records were written over\n", cpu,
            ns / consts::NANO_SECS_IN_SEC, (ns / consts::NANO_SECS_IN_USEC) % USECS_IN_SEC);
    }
}

/* The newest `count` records of every ring (or of cpu `only`'s), merged by
   their cycle counts, of the events in `mask`. Takes control() to read the
   rings, and prints with it let go. */
void show(stdlib::Printer& con, ulong count, ulong only, EventMask mask)
{
    Window windows[MAX_CPUS];
    ulong total;
    {
        stdlib::AutoLock lock(control());
        total = collect(windows, con, count, only, mask);
    }
    u64 now_cycles;
    ulong now_ns;
    now_point(now_cycles, now_ns);

    /* Merged oldest first, the oldest past the newest `count` passed over. */
    ulong skip = (total > count) ? total - count : 0;
    if (total != 0)
        say_cut(con, windows, skip, now_cycles, now_ns);
    const TraceRing::Record* r;
    ulong taken = 0;
    while (merge_next(windows, MAX_CPUS, r))
    {
        if (taken++ < skip)
            continue;
        print_record(con, *r, record_ns(r->cycles, now_cycles, now_ns));
    }

    release_windows(windows);
    if (total == 0)
        nothing_recorded(con);
}

/* ---- `trace dump`: the window as JSON lines, a record a line ---- */

/* `s` as a JSON string: what a task's name is written as. */
void json_string(stdlib::Printer& con, const char* s)
{
    con.printf("\"");
    for (const char* p = s; *p != '\0'; p++)
    {
        auto c = (unsigned char)*p;
        if (c == '"' || c == '\\')
            con.printf("\\%c", c);
        else if (c < 0x20 || c >= 0x7F)
            con.printf("\\u%04x", (unsigned)c);
        else
            con.printf("%c", c);
    }
    con.printf("\"");
}

/* One record as an object: its time in ns since boot, the cpu it was
   recorded on, the event, and its fields as the description has them --
   in an object of their own, as some of them are called cpu too. A hex
   field is its number (JSON has no 0x) and an ns one is its ns, so that
   nothing of a record is rounded before a host sees it. */
void dump_record(stdlib::Printer& con, const TraceRing::Record& r, ulong ns)
{
    con.printf("{\"t\":%lu,\"cpu\":%u", ns, r.cpu);
    if (r.id >= COUNT || r.size != EVENTS[r.id].size)
    {
        /* Written by a kernel this one does not know: its number and size,
           and none of a payload it cannot read */
        con.printf(",\"ev\":null,\"id\":%u,\"size\":%u}\n", (unsigned)r.id, (unsigned)r.size);
        return;
    }

    const Event& e = EVENTS[r.id];
    con.printf(",\"ev\":\"%s\",\"f\":{", e.name);
    for (ulong i = 0; i < e.field_count; i++)
    {
        const Field& f = e.fields[i];
        u64 v = field_value(r, f);
        con.printf("%s\"%s\":", (i == 0) ? "" : ",", f.name);
        switch (f.kind)
        {
        case Kind::Signed:
            con.printf("%ld", (long)v);
            break;
        case Kind::Enum:
            if (v < f.name_count)
                json_string(con, f.names[v]);
            else
                con.printf("%lu", (ulong)v);
            break;
        case Kind::Comm:
        {
            char name[COMM_BYTES + 1];
            read_comm(r, f, name, sizeof(name));
            json_string(con, name);
            break;
        }
        case Kind::Hex:
        case Kind::Ns:
        case Kind::Unsigned:
        case Kind::Cpu:
        case Kind::Pid:
        default:
            con.printf("%lu", (ulong)v);
            break;
        }
    }
    con.printf("}}\n");
}

void dump(stdlib::Printer& con, ulong count, ulong only, EventMask mask)
{
    Window windows[MAX_CPUS];
    ulong total;
    {
        stdlib::AutoLock lock(control());
        total = collect(windows, con, count, only, mask);
    }
    u64 now_cycles;
    ulong now_ns;
    now_point(now_cycles, now_ns);

    if (total == 0)
    {
        release_windows(windows);
        nothing_recorded(con);
        return;
    }

    /* The window first: how many records follow, and from when on none of
       them is missing -- where a reader of the dump may pair waits from, as
       `trace latency` does (scripts/trace-decode.py): 0 when nothing is,
       null when a CPU's part could not be read at all. */
    ulong skip = (total > count) ? total - count : 0;
    ulong limit_cpu;
    u64 start = window_start(windows, MAX_CPUS, limit_cpu);
    con.printf("{\"window\":{\"records\":%lu,\"complete_from\":", total - skip);
    if (start == COMPLETE_NEVER)
        con.printf("null,\"cpu\":%lu}}\n", limit_cpu);
    else if (start == 0)
        con.printf("0,\"cpu\":null}}\n");
    else
        con.printf("%lu,\"cpu\":%lu}}\n", record_ns(start, now_cycles, now_ns), limit_cpu);

    const TraceRing::Record* r;
    ulong taken = 0;
    while (merge_next(windows, MAX_CPUS, r))
    {
        if (taken++ < skip)
            continue;
        dump_record(con, *r, record_ns(r->cycles, now_cycles, now_ns));
    }
    release_windows(windows);
}

/* ---- `trace latency`: how long a woken task waited for its CPU ---- */

/* A wait as text: ns, then us, then ms, then s, three decimals past the
   point -- the units a person reads a scheduler's waits in. */
void wait_text(char* to, ulong len, u64 ns)
{
    if (ns < consts::NANO_SECS_IN_USEC)
        stdlib::sn_printf(to, len, "%luns", (ulong)ns);
    else if (ns < consts::NANO_SECS_IN_MS)
        stdlib::sn_printf(to, len, "%lu.%03luus", (ulong)(ns / consts::NANO_SECS_IN_USEC),
            (ulong)(ns % consts::NANO_SECS_IN_USEC));
    else if (ns < consts::NANO_SECS_IN_SEC)
        stdlib::sn_printf(to, len, "%lu.%03lums", (ulong)(ns / consts::NANO_SECS_IN_MS),
            (ulong)((ns / consts::NANO_SECS_IN_USEC) % 1000));
    else
        stdlib::sn_printf(to, len, "%lu.%03lus", (ulong)(ns / consts::NANO_SECS_IN_SEC),
            (ulong)((ns / consts::NANO_SECS_IN_MS) % 1000));
}

void print_waits(stdlib::Printer& con, const WakePairing& pairing, WakeWait* waits, ulong walked, ulong cpus,
    u64 start, ulong limit_cpu, u64 now_cycles, ulong now_ns);

/* Each wakeup of the window `trace show` prints paired with the first
   switch to its task after it (WakePairing): the waits' percentiles, and
   the longest with their tasks and CPUs. Only from where every CPU's part
   of the window is whole (window_start): before, a wakeup's switch may be
   among what a ring lost. */
void latency(stdlib::Printer& con, ulong count, ulong only, EventMask mask)
{
    Window windows[MAX_CPUS];
    ulong total;
    {
        stdlib::AutoLock lock(control());
        total = collect(windows, con, count, only, mask);
    }
    u64 now_cycles;
    ulong now_ns;
    now_point(now_cycles, now_ns);

    ulong cpus = 0;
    for (ulong cpu = 0; cpu < MAX_CPUS; cpu++)
        cpus += (windows[cpu].count != 0) ? 1 : 0;

    ulong limit_cpu;
    u64 start = window_start(windows, MAX_CPUS, limit_cpu);
    if (start == COMPLETE_NEVER)
    {
        con.printf("trace latency: cpu %lu's ring could not be read: no wait is paired across it\n", limit_cpu);
        release_windows(windows);
        return;
    }

    /* The walk meets at most the window's newest `count`, and a wait is a
       switch among them: room for that many */
    ulong walked = (total < count) ? total : count;
    WakeWait* waits = nullptr;
    if (walked != 0)
    {
        waits = static_cast<WakeWait*>(mm::alloc(walked * sizeof(WakeWait), TAG));
        if (waits == nullptr)
        {
            con.printf("trace latency: no memory to hold %lu waits\n", walked);
            release_windows(windows);
            return;
        }
    }

    /* A pid's table of a few KiB: the heap's, not the shell's stack */
    auto pairing = new (mm::NO_THROW) WakePairing(waits, walked, start, cycle_counter_delta_to_ns);
    if (pairing == nullptr)
    {
        con.printf("trace latency: no memory to pair waits\n");
        if (waits != nullptr)
            mm::free(waits);
        release_windows(windows);
        return;
    }
    ulong skip = total - walked;
    const TraceRing::Record* r;
    ulong taken = 0;
    while (merge_next(windows, MAX_CPUS, r))
    {
        if (taken++ < skip)
            continue;
        pairing->feed(*r);
    }
    release_windows(windows);
    print_waits(con, *pairing, waits, walked, cpus, start, limit_cpu, now_cycles, now_ns);
    delete pairing;
    if (waits != nullptr)
        mm::free(waits);
}

/* What latency paired, said: the counts, why some were left out, the
   percentiles and the longest five. */
void print_waits(stdlib::Printer& con, const WakePairing& pairing, WakeWait* waits, ulong walked, ulong cpus,
    u64 start, ulong limit_cpu, u64 now_cycles, ulong now_ns)
{
    ulong ran = pairing.waits();
    con.printf("trace latency: %lu records on %lu cpus -- %lu wakeups, %lu ran in the window, %lu not run in it\n",
        walked, cpus, pairing.wakeups(), ran, pairing.pending());
    if (pairing.before_start() != 0)
    {
        ulong ns = record_ns(start, now_cycles, now_ns);
        con.printf("  the oldest %lu left out: cpu %lu holds nothing before %lu.%06lu -- its older records were "
            "written over\n", pairing.before_start(), limit_cpu, ns / consts::NANO_SECS_IN_SEC,
            (ns / consts::NANO_SECS_IN_USEC) % USECS_IN_SEC);
    }
    if (pairing.superseded() != 0)
        con.printf("  %lu wakeups were followed by another of their task's before any switch to it\n",
            pairing.superseded());
    if (pairing.strange() != 0)
        con.printf("  %lu wakeups named a pid past the table's %lu, and are left out\n", pairing.strange(),
            WakePairing::PID_SLOTS);
    if (pairing.dropped() != 0)
        con.printf("  %lu waits found no room, and are left out\n", pairing.dropped());
    if (ran == 0)
    {
        if (walked != 0)
            con.printf("  no wakeup of the window was followed by its task's switch -- "
                "`trace on sched_switch,sched_wakeup`, and a window with both in it\n");
        return;
    }

    /* p50, p90, p99 and the most: the waits sorted, so that a percentile is
       one index -- a histogram's would be its buckets' bounds */
    auto sorted = static_cast<u32*>(mm::alloc(ran * sizeof(u32), TAG));
    if (sorted == nullptr)
    {
        con.printf("  no memory to sort %lu waits\n", ran);
        return;
    }
    for (ulong i = 0; i < ran; i++)
        sorted[i] = waits[i].ns;
    sort_u32(sorted, ran);

    const ulong P50 = 50;
    const ulong P90 = 90;
    const ulong P99 = 99;
    char p50[32];
    char p90[32];
    char p99[32];
    char pmax[32];
    wait_text(p50, sizeof(p50), sorted[percentile_index(ran, P50)]);
    wait_text(p90, sizeof(p90), sorted[percentile_index(ran, P90)]);
    wait_text(p99, sizeof(p99), sorted[percentile_index(ran, P99)]);
    wait_text(pmax, sizeof(pmax), sorted[ran - 1]);
    con.printf("  wait p50 %s p90 %s p99 %s max %s\n", p50, p90, p99, pmax);

    /* The five longest, each with its task and CPUs: what a histogram
       cannot say */
    const ulong WORST = 5;
    ulong worst[WORST];
    ulong n = top_waits(waits, ran, worst, WORST);
    con.printf("  the longest:\n");
    for (ulong i = 0; i < n; i++)
    {
        const WakeWait& ww = waits[worst[i]];
        char text[32];
        wait_text(text, sizeof(text), ww.ns);
        con.printf("    %9s pid %u %-12s cpu %u -> %u\n", text, ww.pid, ww.comm, ww.woken_cpu, ww.ran_cpu);
    }
    mm::free(sorted);
}

void stat(stdlib::Printer& con)
{
    const ulong kib = ring_size_kib();
    con.printf("trace: rings of %lu KiB a cpu (%lu records)%s%s\n", kib, kib * consts::KB / TraceRing::SLOT_BYTES,
        have_rings() ? "" : ", none made yet",
        __atomic_load_n(&frozen, __ATOMIC_RELAXED) ? ", frozen" : "");

    u64 armed = __atomic_load_n(&wakeup_trigger_ns, __ATOMIC_RELAXED);
    if (armed != 0)
    {
        char text[32];
        wait_text(text, sizeof(text), armed);
        con.printf("trace: a wakeup trigger is armed: the rings freeze when a wakeup waits over %s\n", text);
    }

    WakeupTrigger t;
    bool busy;
    if (triggered(t, busy))
    {
        char text[32];
        char name[COMM_BYTES + 1] = {};
        stdlib::mem_cpy(name, t.comm, COMM_BYTES);
        wait_text(text, sizeof(text), t.waited_ns);
        ulong at = (ulong)t.at_ns;
        con.printf("trace: frozen by the wakeup trigger: pid %u %s waited %s on cpu %u, at %lu.%06lu\n",
            (u32)(t.pid_cpu >> 32), name, text, (u32)t.pid_cpu, at / consts::NANO_SECS_IN_SEC,
            (at / consts::NANO_SECS_IN_USEC) % USECS_IN_SEC);
    }
    else if (busy)
    {
        con.printf("trace: the wakeup trigger's record was being written at every look: ask again\n");
    }

    con.printf("cpu      written       kept    dropped\n");
    for (ulong cpu = 0; cpu < MAX_CPUS; cpu++)
    {
        TraceRing* ring = __atomic_load_n(&rings[cpu], __ATOMIC_ACQUIRE);
        if (ring == nullptr)
            continue;
        u64 head = ring->head();
        u64 kept = head - cleared_at[cpu];
        if (kept > ring->capacity())
            kept = ring->capacity();
        con.printf("%3lu %12lu %10lu %10lu\n", cpu, (ulong)head, (ulong)kept, (ulong)ring->dropped());
    }
}

void list(stdlib::Printer& con)
{
    for (ulong i = 0; i < COUNT; i++)
    {
        const Event& e = EVENTS[i];
        con.printf("  %s %-22s %-8s %s\n", on((Id)i) ? "on " : "off", e.name, e.group, e.help);
    }
}

/* `trace test [n]`: n Test records on this CPU -- one CPU's: preemption is
   off for the loop, so that nothing moves the shell to another CPU midway
   and the records stay in the ring the line names. */
void test_records(stdlib::Printer& con, ulong n)
{
    if (!on(Id::Test))
    {
        con.printf("trace: `trace on test` first\n");
        return;
    }
    if (__atomic_load_n(&frozen, __ATOMIC_RELAXED))
    {
        con.printf("trace: the rings are frozen -- `trace thaw` first\n");
        return;
    }
    preempt_disable();
    ulong cpu = CpuTable::get_instance().get_current_cpu_id();
    for (ulong i = 0; i < n; i++)
    {
        Test t = { i, i * 0x9E3779B97F4A7C15ULL };
        emit(Id::Test, &t, sizeof(t));
    }
    preempt_enable();
    con.printf("trace: %lu test records on cpu %lu\n", n, cpu);
}

/* A record of event `n` into the ring `table` has for this CPU, as emit()
   writes one: with interrupts off, from the table's slot for the CPU it
   runs on. The kernel's rings, or `trace bench`'s own. */
void write_through(TraceRing** table, ulong n, const void* payload, ulong size)
{
    ulong flags = hal::irq_save();
    ulong cpu = hal::get_current_cpu_hw_id();
    if (cpu < MAX_CPUS)
    {
        TraceRing* ring = __atomic_load_n(&table[cpu], __ATOMIC_ACQUIRE);
        if (ring != nullptr)
            ring->write((u16)n, hal::read_cycle_counter(), payload, size);
    }
    hal::irq_restore(flags);
}

/* `trace bench`: what a tracepoint costs this CPU off, and on. The one that
   is on writes into a ring of its own -- every CPU's slot of a table of its
   own pointing at it, the shell's task its only writer -- so that what the
   kernel's rings hold stays as it was, frozen or not. */
void bench(stdlib::Printer& con)
{
    static const ulong OFF_LOOPS = 10000000;
    static const ulong ON_LOOPS = 100000;
    bool was_on = on(Id::Test);
    ulong hits = 0;

    __atomic_store_n(&enabled[(ulong)Id::Test], 0, __ATOMIC_RELEASE);
    u64 start = hal::read_cycle_counter();
    for (ulong i = 0; i < OFF_LOOPS; i++)
    {
        if (unlikely(on(Id::Test)))
            hits++;
        hal::compiler_barrier();
    }
    u64 off_cycles = hal::read_cycle_counter() - start;

    start = hal::read_cycle_counter();
    for (ulong i = 0; i < OFF_LOOPS; i++)
        hal::compiler_barrier();
    u64 empty_cycles = hal::read_cycle_counter() - start;

    ulong off_ps = cycle_counter_delta_to_ns(off_cycles > empty_cycles ? off_cycles - empty_cycles : 0) * 1000 / OFF_LOOPS;
    con.printf("trace bench: a tracepoint that is off, %lu.%03lu ns over an empty loop's iteration (%lu loops)\n",
        off_ps / 1000, off_ps % 1000, OFF_LOOPS);

    __atomic_store_n(&enabled[(ulong)Id::Test], was_on ? 1 : 0, __ATOMIC_RELEASE);
    (void)hits;

    TraceRing ring;
    if (!ring.init(1, TAG))
    {
        con.printf("trace bench: no memory for a ring of its own, to time one that is on\n");
        return;
    }
    TraceRing* table[MAX_CPUS];
    for (ulong i = 0; i < MAX_CPUS; i++)
        table[i] = &ring;

    start = hal::read_cycle_counter();
    for (ulong i = 0; i < ON_LOOPS; i++)
    {
        Test t = { i, i };
        write_through(table, (ulong)Id::Test, &t, sizeof(t));
    }
    u64 on_cycles = hal::read_cycle_counter() - start;
    ulong on_ps = cycle_counter_delta_to_ns(on_cycles) * 1000 / ON_LOOPS;
    con.printf("trace bench: one that is on, %lu.%03lu ns a record (%lu records into a ring of its own)\n",
        on_ps / 1000, on_ps % 1000, ON_LOOPS);
    ring.release();
}

/* The words of `args` past the first, each into `words` (up to `max`, of
   `len` bytes each). */
const ulong WORDS_MAX = 16;
const ulong WORD_LEN = 32;

ulong split(const char* args, char words[WORDS_MAX][WORD_LEN])
{
    ulong n = 0;
    const char* end;
    for (const char* tok = stdlib::next_token(args, end); tok && n < WORDS_MAX; tok = stdlib::next_token(end, end))
    {
        char token[WORDS_MAX * WORD_LEN];
        stdlib::token_copy(tok, end, token, sizeof(token));
        /* Commas split words too, as spaces do: `trace on
           sched_switch,sched_wakeup`. An empty word between two is none. */
        const char* from = token;
        for (;;)
        {
            const char* comma = from;
            while (*comma != '\0' && *comma != ',')
                comma++;
            ulong len = (ulong)(comma - from);
            if (len != 0 && n < WORDS_MAX)
            {
                if (len >= WORD_LEN)
                    len = WORD_LEN - 1;
                stdlib::mem_cpy(words[n], from, len);
                words[n][len] = '\0';
                n++;
            }
            if (*comma == '\0')
                break;
            from = comma + 1;
        }
    }
    return n;
}

}

void emit(Id id, const void* payload, ulong size)
{
    ulong n = (ulong)id;
    if (BUG_ON(n >= COUNT || size != EVENTS[n].size))
        return;
    if (__atomic_load_n(&frozen, __ATOMIC_RELAXED))
        return;
    write_through(rings, n, payload, size);
}

bool set_ring_kib(ulong kib, stdlib::Printer& con)
{
    if (!valid_ring_kib(kib))
    {
        con.printf("trace: a ring is %lu KiB times a power of two, up to %lu KiB (now %lu)\n",
            MIN_RING_KIB, MAX_RING_KIB, ring_size_kib());
        return false;
    }
    stdlib::AutoLock lock(control());
    /* Atomics: ring_size_kib() reads it without the lock. */
    ulong was = __atomic_load_n(&ring_kib, __ATOMIC_RELAXED);
    __atomic_store_n(&ring_kib, kib, __ATOMIC_RELAXED);
    /* Rings made already are made again at the new size: what they held
       goes with them, and so does a freeze, or a trigger armed to keep it */
    if (have_rings())
    {
        if (!make_rings(con))
        {
            __atomic_store_n(&ring_kib, was, __ATOMIC_RELAXED);
            return false;
        }
        thaw();
    }
    return true;
}

ulong ring_size_kib()
{
    return __atomic_load_n(&ring_kib, __ATOMIC_RELAXED);
}

void free_rings()
{
    stdlib::AutoLock lock(control());
    for (ulong i = 0; i < COUNT; i++)
        __atomic_store_n(&enabled[i], 0, __ATOMIC_RELEASE);
    /* As it was before the first `trace on`: no freeze, no trigger, so that
       the rings the next one makes record */
    thaw();
    TraceRing* none[MAX_CPUS] = {};
    swap_rings(none);
}

ulong read_cpu(ulong cpu, Id id, TraceRing::Record* out, ulong max)
{
    if (cpu >= MAX_CPUS || max == 0)
        return 0;
    stdlib::AutoLock lock(control());
    TraceRing* ring = __atomic_load_n(&rings[cpu], __ATOMIC_ACQUIRE);
    if (ring == nullptr)
        return 0;

    u64 head = ring->head();
    u64 from = cleared_at[cpu];
    if (head > ring->capacity() && head - ring->capacity() > from)
        from = head - ring->capacity();

    /* The newest `max` of `id`'s: the ring walked from its newest back, the
       ones kept turned round after. */
    ulong got = 0;
    for (u64 index = head; index > from && got < max; index--)
    {
        TraceRing::Record r;
        if (!ring->read(index - 1, r) || r.id != (u16)id)
            continue;
        r.cpu = (u32)cpu;
        out[got++] = r;
    }
    for (ulong i = 0; i < got / 2; i++)
    {
        TraceRing::Record t = out[i];
        out[i] = out[got - 1 - i];
        out[got - 1 - i] = t;
    }
    return got;
}

const Event& event(Id id)
{
    return EVENTS[(ulong)id < COUNT ? (ulong)id : 0];
}

long set(const char* what, bool turn_on, stdlib::Printer& con)
{
    char words[WORDS_MAX][WORD_LEN] = {};
    ulong n = split(what, words);
    if (n == 0)
    {
        con.printf("trace: name events, groups or `all`\n");
        return -1;
    }

    EventMask chosen = 0;
    for (ulong w = 0; w < n; w++)
    {
        if (!name_mask(words[w], chosen))
            con.printf("trace: no event or group called %s\n", words[w]);
    }
    long named = 0;
    for (ulong i = 0; i < COUNT; i++)
        named += ((chosen & event_bit(i)) != 0) ? 1 : 0;
    if (named == 0)
        return -1;

    stdlib::AutoLock lock(control());
    if (turn_on && !have_rings() && !make_rings(con))
        return -1;
    for (ulong i = 0; i < COUNT; i++)
    {
        if ((chosen & event_bit(i)) != 0)
            __atomic_store_n(&enabled[i], turn_on ? 1 : 0, __ATOMIC_RELEASE);
    }
    /* On, and recording nothing, would be a trace that looks empty */
    if (turn_on && __atomic_load_n(&frozen, __ATOMIC_RELAXED))
        con.printf("trace: the rings are frozen -- nothing is recorded until `trace thaw`\n");
    return named;
}

void boot_on(const char* list)
{
    if (list == nullptr || list[0] == '\0')
        return;

    /* The boot's printer: whatever it says goes to the log. */
    class LogPrinter final : public stdlib::Printer
    {
    public:
        PRINTF_FORMAT(2, 3) void printf(const char* fmt, ...) override
        {
            va_list args;
            va_start(args, fmt);
            v_printf(fmt, args);
            va_end(args);
        }
        PRINTF_FORMAT(2, 0) void v_printf(const char* fmt, va_list args) override
        {
            char line[256];
            int len = stdlib::vsn_printf(line, sizeof(line), fmt, args);
            /* klog ends the line itself */
            if (len > 0 && (size_t)len < sizeof(line) && line[len - 1] == '\n')
                line[len - 1] = '\0';
            klog(0, "%s", line);
        }
        void print_string(const char* s) override
        {
            klog(0, "%s", s);
        }
        void backspace() override
        {
        }
    };
    LogPrinter log;

    long named = set(list, true, log);
    if (named > 0)
        klog(0, "trace: %ld events on from boot (tracepoints=%s)", named, list);
}

/* show's, dump's and latency's arguments: [n] [cpu=N] [event|group|all...]. */
struct WindowArgs
{
    ulong count;
    ulong only;
    EventMask mask;
};

/* The window the words after the verb ask for -- false, said, for a word
   that is none of n (1 up; SHOW_MAX at most, more taken as it), cpu=N of a
   running CPU, or what `trace on` takes: a word that names nothing is a
   mistake to say, not a filter that silently matches nothing. */
bool window_args(char words[WORDS_MAX][WORD_LEN], ulong n, WindowArgs& out, stdlib::Printer& con)
{
    const ulong CPU_PREFIX_LEN = 4;
    out.count = SHOW_DEFAULT;
    out.only = MAX_CPUS;
    out.mask = 0;
    for (ulong w = 1; w < n; w++)
    {
        ulong value;
        if (stdlib::mem_cmp(words[w], "cpu=", CPU_PREFIX_LEN) == 0)
        {
            if (!stdlib::parse_ulong(words[w] + CPU_PREFIX_LEN, value) || value >= MAX_CPUS
                || (CpuTable::get_instance().get_running_cpus() & (1UL << value)) == 0)
            {
                con.printf("trace: no running cpu %s\n", words[w] + CPU_PREFIX_LEN);
                return false;
            }
            out.only = value;
        }
        else if (stdlib::parse_ulong(words[w], value))
        {
            if (value == 0)
            {
                con.printf("trace: a window is 1 to %lu records\n", SHOW_MAX);
                return false;
            }
            out.count = (value < SHOW_MAX) ? value : SHOW_MAX;
        }
        else if (!name_mask(words[w], out.mask))
        {
            con.printf("trace: no event or group called %s\n", words[w]);
            return false;
        }
    }
    if (out.mask == 0)
        out.mask = ALL_EVENTS;
    return true;
}

/* `wakeup>N` and a unit -- ns, us, ms or s -- as `trace freeze` takes it. */
bool parse_wakeup_trigger(const char* word, u64& ns)
{
    const ulong PREFIX_LEN = 7;
    if (stdlib::mem_cmp(word, "wakeup>", PREFIX_LEN) != 0)
        return false;
    return parse_duration(word + PREFIX_LEN, ns);
}

void command(const char* args, stdlib::Printer& con)
{
    char words[WORDS_MAX][WORD_LEN] = {};
    ulong n = split(args, words);
    const char* verb = (n == 0) ? "list" : words[0];

    /* The rest of the line after the verb, for set(). */
    const char* rest = args;
    {
        const char* end;
        const char* tok = stdlib::next_token(args, end);
        rest = (tok != nullptr) ? end : args;
    }

    if (stdlib::str_cmp(verb, "list") == 0)
    {
        list(con);
    }
    else if (stdlib::str_cmp(verb, "on") == 0 || stdlib::str_cmp(verb, "off") == 0)
    {
        long named = set(rest, stdlib::str_cmp(verb, "on") == 0, con);
        if (named > 0)
            con.printf("trace: %ld events %s\n", named, verb);
    }
    else if (stdlib::str_cmp(verb, "show") == 0 || stdlib::str_cmp(verb, "dump") == 0
        || stdlib::str_cmp(verb, "latency") == 0)
    {
        /* Each takes control() to read the rings, and prints with it let go */
        WindowArgs window;
        if (!window_args(words, n, window, con))
            return;
        if (stdlib::str_cmp(verb, "show") == 0)
            show(con, window.count, window.only, window.mask);
        else if (stdlib::str_cmp(verb, "dump") == 0)
            dump(con, window.count, window.only, window.mask);
        else
            latency(con, window.count, window.only, window.mask);
    }
    else if (stdlib::str_cmp(verb, "stat") == 0)
    {
        stdlib::AutoLock lock(control());
        stat(con);
    }
    else if (stdlib::str_cmp(verb, "clear") == 0)
    {
        stdlib::AutoLock lock(control());
        for (ulong cpu = 0; cpu < MAX_CPUS; cpu++)
        {
            TraceRing* ring = __atomic_load_n(&rings[cpu], __ATOMIC_ACQUIRE);
            if (ring != nullptr)
                cleared_at[cpu] = ring->head();
        }
        con.printf("trace: cleared\n");
    }
    else if (stdlib::str_cmp(verb, "size") == 0)
    {
        ulong kib;
        if (n < 2 || !stdlib::parse_ulong(words[1], kib))
            kib = 0;
        if (set_ring_kib(kib, con))
            con.printf("trace: rings of %lu KiB a cpu\n", kib);
    }
    else if (stdlib::str_cmp(verb, "free") == 0)
    {
        free_rings();
        con.printf("trace: every event off, the rings freed\n");
    }
    else if (stdlib::str_cmp(verb, "freeze") == 0 || stdlib::str_cmp(verb, "thaw") == 0)
    {
        bool freeze = stdlib::str_cmp(verb, "freeze") == 0;
        u64 over = 0;
        if (freeze && n >= 2)
        {
            if (!parse_wakeup_trigger(words[1], over))
            {
                con.printf("usage: trace freeze [wakeup>N<ns|us|ms|s>]\n");
                return;
            }

            stdlib::AutoLock lock(control());
            arm_trigger(over);
            char text[32];
            wait_text(text, sizeof(text), over);
            con.printf("trace: armed -- the rings freeze when a wakeup waits over %s\n", text);
            return;
        }

        /* A freeze keeps what a trigger fired on, for `trace stat`; a thaw
           forgets it */
        stdlib::AutoLock lock(control());
        if (freeze)
        {
            disarm_trigger(false);
            __atomic_store_n(&frozen, 1, __ATOMIC_RELEASE);
            con.printf("trace: frozen -- nothing more is recorded until `trace thaw`\n");
        }
        else
        {
            thaw();
            con.printf("trace: recording\n");
        }
    }
    else if (stdlib::str_cmp(verb, "test") == 0)
    {
        ulong count = 10;
        if (n >= 2 && (!stdlib::parse_ulong(words[1], count) || count == 0 || count > TraceRing::SLOTS_PER_CHUNK * 16))
        {
            con.printf("usage: trace test [n]\n");
            return;
        }
        test_records(con, count);
    }
    else if (stdlib::str_cmp(verb, "bench") == 0)
    {
        stdlib::AutoLock lock(control());
        bench(con);
    }
    else
    {
        con.printf("usage: trace [list] | on <event|group|all>... | off <...> | show [n] [cpu=N] [event|group|all...] | "
            "dump [n] [cpu=N] [event|group|all...] | latency [n] [cpu=N] [event|group|all...] | stat | clear | "
            "size <KiB> | free | freeze [wakeup>N<ns|us|ms|s>] | thaw | test [n] | bench\n");
    }
}

/* ---- the scheduler's sites ---- */

namespace
{

/* A task's name into a payload's field: what fits, zeroed past the name's
   end, so that no byte of a record is whatever the stack held. Its name is
   written once, when the task is made, and never changed after. */
void put_comm(char* to, const char* name)
{
    stdlib::mem_set(to, 0, COMM_BYTES);
    if (name == nullptr)
        return;
    ulong len = stdlib::str_len(name);
    if (len > COMM_BYTES)
        len = COMM_BYTES;
    stdlib::mem_cpy(to, name, len);
}

u32 task_pid(Task* task)
{
    return (task != nullptr) ? (u32)task->pid_ : NO_TASK_PID;
}

/* A task's name as a task says it, or none. Reading it takes no lock: it is
   written when the task is made, before anything can find it. */
const char* task_name(Task* task)
{
    return (task != nullptr) ? task->get_name() : nullptr;
}

}

void emit_sched_switch(Task* prev, Task* next, ulong cpu, u8 state, u8 reason)
{
    SchedSwitch s = {};
    s.prev_pid = task_pid(prev);
    put_comm(s.prev_comm, task_name(prev));
    s.next_pid = task_pid(next);
    put_comm(s.next_comm, task_name(next));
    s.next_cpu = (u32)cpu;
    s.prev_state = state;
    s.reason = reason;
    emit(Id::SchedSwitch, &s, sizeof(s));
}

void emit_sched_wakeup(Task* task, ulong cpu, bool deadline, Task* waker, ulong late_ns)
{
    SchedWakeup w = {};
    w.pid = task_pid(task);
    put_comm(w.comm, task_name(task));
    w.cpu = (u32)cpu;
    w.deadline = deadline ? 1 : 0;
    w.waker_pid = task_pid(waker);
    w.late_ns = (u32)stdlib::min(late_ns, (ulong)~0U);
    emit(Id::SchedWakeup, &w, sizeof(w));
}

void emit_sched_block(Task* task, ulong cpu, ulong ran_ns)
{
    SchedBlock b = {};
    b.pid = task_pid(task);
    put_comm(b.comm, task_name(task));
    b.cpu = (u32)cpu;
    b.deadline = ((ulong)task->sleep_until_.get() != 0) ? 1 : 0;
    b.ran_ns = (u32)stdlib::min(ran_ns, (ulong)~0U);
    emit(Id::SchedBlock, &b, sizeof(b));
}

void emit_sched_migrate(Task* task, ulong from, ulong to)
{
    SchedMigrate m = {};
    m.pid = task_pid(task);
    put_comm(m.comm, task_name(task));
    m.from = (u32)from;
    m.to = (u32)to;
    emit(Id::SchedMigrate, &m, sizeof(m));
}

void emit_sched_new(Task* task, ulong cpu)
{
    SchedNew n = {};
    n.pid = task_pid(task);
    put_comm(n.comm, task_name(task));
    n.cpu = (u32)cpu;
    emit(Id::SchedNew, &n, sizeof(n));
}

void emit_sched_exit(Task* task, ulong cpu)
{
    SchedExit e = {};
    e.pid = task_pid(task);
    put_comm(e.comm, task_name(task));
    e.cpu = (u32)cpu;
    emit(Id::SchedExit, &e, sizeof(e));
}

void emit_sched_ipi(ulong cpu, ulong tasks)
{
    SchedIpi i = {};
    i.cpu = (u32)cpu;
    i.tasks = (u32)tasks;
    emit(Id::SchedIpi, &i, sizeof(i));
}

void emit_sched_preempt_deferred(Task* task, ulong cpu, ulong count, bool idle)
{
    SchedPreemptDeferred d = {};
    d.pid = task_pid(task);
    put_comm(d.comm, task_name(task));
    d.cpu = (u32)cpu;
    d.count = (u32)count;
    d.idle = idle ? 1 : 0;
    emit(Id::SchedPreemptDeferred, &d, sizeof(d));
}

void emit_idle(Id id, ulong cpu, Task* idle_task)
{
    Idle i = {};
    i.cpu = (u32)cpu;
    i.pid = task_pid(idle_task);
    emit(id, &i, sizeof(i));
}

void emit_sched_slice(ulong cpu, Task* task, ulong slice_ns, ulong runnable)
{
    SchedSlice s = {};
    s.cpu = (u32)cpu;
    s.pid = task_pid(task);
    s.slice_ns = (u32)stdlib::min(slice_ns, (ulong)~0U);
    s.runnable = (u32)runnable;
    emit(Id::SchedSlice, &s, sizeof(s));
}

/* A wakeup's wait, from the scheduler (TaskQueue::note_run), which has it in
   its hand: the rings are frozen, once, if it is over the trigger's. The
   first CPU to claim writes the record the others see; a second wakeup over
   the threshold at the same moment finds it claimed and does nothing. */
void note_wakeup(ulong waited_ns, ulong now_ns, Task* task, ulong cpu)
{
    u64 threshold = __atomic_load_n(&wakeup_trigger_ns, __ATOMIC_RELAXED);
    if (threshold == 0 || waited_ns < threshold)
        return;

    if (__atomic_exchange_n(&trigger.claim, 1, __ATOMIC_ACQ_REL) != 0)
        return;

    /* The record, a seqlock's write: odd while it goes in, even after */
    u32 seq = __atomic_load_n(&trigger.seq, __ATOMIC_RELAXED);
    __atomic_store_n(&trigger.seq, seq + 1, __ATOMIC_RELAXED);
    hal::smp_wmb();
    __atomic_store_n(&trigger.waited_ns, waited_ns, __ATOMIC_RELAXED);
    __atomic_store_n(&trigger.at_ns, now_ns, __ATOMIC_RELAXED);
    __atomic_store_n(&trigger.pid_cpu, ((u64)task_pid(task) << 32) | (u64)(u32)cpu, __ATOMIC_RELAXED);
    {
        u64 words[2] = {};
        put_comm(reinterpret_cast<char*>(words), task_name(task));
        __atomic_store_n(&trigger.comm[0], words[0], __ATOMIC_RELAXED);
        __atomic_store_n(&trigger.comm[1], words[1], __ATOMIC_RELAXED);
    }
    hal::smp_wmb();
    __atomic_store_n(&trigger.seq, seq + 2, __ATOMIC_RELEASE);

    /* Fired is done: the rings are frozen, and no wakeup has anything more
       to trigger. After the record, so that whoever finds the rings frozen
       can read what froze them. */
    __atomic_store_n(&frozen, 1, __ATOMIC_RELEASE);
    __atomic_store_n(&wakeup_trigger_ns, 0, __ATOMIC_RELEASE);
}

}
}
