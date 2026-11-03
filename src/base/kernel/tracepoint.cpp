#include "tracepoint.h"
#include "trace_ring.h"

#include "cpu.h"
#include "klog.h"
#include "mutex.h"
#include "panic.h"
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

const Event EVENTS[COUNT] = {
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
    { "sched_exit", "sched", "a task that has exited, and whose CPU takes its last reference and its stack",
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
    { "idle_exit", "idle", "that CPU back from the halt",
      sizeof(Idle), sizeof(IDLE_FIELDS) / sizeof(IDLE_FIELDS[0]), IDLE_FIELDS },
    { "sched_slice", "debug", "the slice the current task was given, from the decision that armed this CPU's timer "
      "for its deadline",
      sizeof(SchedSlice), sizeof(SCHED_SLICE_FIELDS) / sizeof(SCHED_SLICE_FIELDS[0]), SCHED_SLICE_FIELDS },
};


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

/* Disarm the trigger: the next one armed starts from nothing. Task context
   (`trace freeze`, `trace thaw`); a firing already claimed finishes. */
void disarm_trigger()
{
    __atomic_store_n(&wakeup_trigger_ns, 0, __ATOMIC_RELEASE);
    __atomic_store_n(&trigger.claim, 0, __ATOMIC_RELAXED);
    __atomic_store_n(&trigger.seq, 0, __ATOMIC_RELEASE);
}

/* What the trigger fired on, if it has: a seqlock read of the record, as a
   ring slot's is. False before any firing. */
bool triggered(WakeupTrigger& out)
{
    for (;;)
    {
        u32 seq = __atomic_load_n(&trigger.seq, __ATOMIC_ACQUIRE);
        if (seq == 0)
            return false;
        if ((seq & 1) != 0)
            continue;
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

/* One CPU's records for `trace show`: the newest `want` of them still in its
   ring, oldest first. */
struct Window
{
    TraceRing::Record* records;
    ulong count;
    ulong next;
};

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

/* Whether `name` is in the comma- or space-separated list of `filter`'s
   words; every name is, for no filter. */
bool name_in(const char* name, const char* const* filter, ulong filter_count)
{
    if (filter_count == 0)
        return true;
    for (ulong i = 0; i < filter_count; i++)
    {
        if (stdlib::str_cmp(filter[i], name) == 0)
            return true;
    }
    return false;
}

/* The newest `count` records of every ring (or of cpu `only`), those of the
   events `filter` names, per CPU and oldest first: what show, dump and
   latency walk. Returns how many there are, across the CPUs. Under
   control(). A CPU whose ring would not fit in memory is left out, said. */
ulong collect(Window windows[MAX_CPUS], stdlib::Printer& con, ulong count, ulong only,
    const char* const* filter, ulong filter_count)
{
    ulong total = 0;
    for (ulong cpu = 0; cpu < MAX_CPUS; cpu++)
    {
        windows[cpu] = {};
        TraceRing* ring = __atomic_load_n(&rings[cpu], __ATOMIC_ACQUIRE);
        if (ring == nullptr || (only != MAX_CPUS && cpu != only))
            continue;

        u64 head = ring->head();
        u64 from = cleared_at[cpu];
        if (head > ring->capacity() && head - ring->capacity() > from)
            from = head - ring->capacity();
        /* A filter's records are found among more than `count`: the window
           is the whole ring then, bounded by what is printed at most. */
        ulong span = (filter_count == 0) ? count : SHOW_MAX;
        if (head > from && head - from > span)
            from = head - span;
        if (head <= from)
            continue;

        ulong want = (ulong)(head - from);
        auto records = static_cast<TraceRing::Record*>(mm::alloc(want * sizeof(TraceRing::Record), TAG));
        if (records == nullptr)
        {
            con.printf("trace: no memory to read cpu %lu's ring\n", cpu);
            continue;
        }

        ulong kept = 0;
        for (u64 index = from; index < head; index++)
        {
            TraceRing::Record& r = records[kept];
            if (!ring->read(index, r))
                continue;
            if (r.id < COUNT && !name_in(EVENTS[r.id].name, filter, filter_count))
                continue;
            r.cpu = (u32)cpu;
            kept++;
        }
        windows[cpu] = { records, kept, 0 };
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
    }
}

/* The next record of the windows, oldest first across the CPUs: what a walk
   of them takes one at a time. False when they are spent. */
bool next_record(Window windows[MAX_CPUS], const TraceRing::Record*& out)
{
    ulong best = MAX_CPUS;
    for (ulong cpu = 0; cpu < MAX_CPUS; cpu++)
    {
        const Window& w = windows[cpu];
        if (w.next >= w.count)
            continue;
        if (best == MAX_CPUS || w.records[w.next].cycles < windows[best].records[windows[best].next].cycles)
            best = cpu;
    }
    if (best == MAX_CPUS)
        return false;
    out = &windows[best].records[windows[best].next++];
    return true;
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

/* The newest `count` records of every ring (or of cpu `only`), merged by
   their cycle counts, of the events `filter` names. Under control(). */
void show(stdlib::Printer& con, ulong count, ulong only, const char* const* filter, ulong filter_count)
{
    Window windows[MAX_CPUS] = {};
    ulong total = collect(windows, con, count, only, filter, filter_count);
    u64 now_cycles;
    ulong now_ns;
    now_point(now_cycles, now_ns);

    /* Merged oldest first, the oldest past the newest `count` passed over. */
    ulong skip = (total > count) ? total - count : 0;
    const TraceRing::Record* r;
    ulong taken = 0;
    while (next_record(windows, r))
    {
        if (taken++ < skip)
            continue;
        print_record(con, *r, record_ns(r->cycles, now_cycles, now_ns));
    }

    release_windows(windows);
    if (total == 0)
        con.printf("trace: nothing recorded%s\n", have_rings() ? "" : " -- no event has been turned on");
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

void dump(stdlib::Printer& con, ulong count, ulong only, const char* const* filter, ulong filter_count)
{
    Window windows[MAX_CPUS] = {};
    ulong total = collect(windows, con, count, only, filter, filter_count);
    u64 now_cycles;
    ulong now_ns;
    now_point(now_cycles, now_ns);

    ulong skip = (total > count) ? total - count : 0;
    const TraceRing::Record* r;
    ulong taken = 0;
    while (next_record(windows, r))
    {
        if (taken++ < skip)
            continue;
        dump_record(con, *r, record_ns(r->cycles, now_cycles, now_ns));
    }
    release_windows(windows);
    if (total == 0)
        con.printf("trace: nothing recorded%s\n", have_rings() ? "" : " -- no event has been turned on");
}

/* ---- `trace latency`: how long a woken task waited for its CPU ---- */

/* The pids a task's can be: ObjectTable's ids, 0 to 255 -- its
   MAX_OBJECT_ID, which it does not lend out. A wakeup's table by pid, then,
   with no map and no allocation on the walk. */
const ulong PID_SLOTS = 256;

/* One wakeup that a switch in the window answered. */
struct WakeWait
{
    /* Clamped to what a u32 holds of ns */
    u32 ns;
    u32 pid;
    u16 woken_cpu;
    u16 ran_cpu;
    /* The task's name, terminated for the print */
    char comm[COMM_BYTES + 1];
};

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

/* One range on the sort's own stack: checked before it is written, so that a
   count past what the stack holds is a report rather than a smashed frame --
   the smaller-half rule keeps it within log2 of the count, 13 for the 4096
   records a window is at most. */
bool push_range(long* stack_lo, long* stack_hi, ulong& top, ulong most, long lo, long hi)
{
    if (top >= most)
    {
        BUG_ON(top >= most);
        return false;
    }
    stack_lo[top] = lo;
    stack_hi[top] = hi;
    top++;
    return true;
}

/* The array, smallest first: quicksort, the smaller half sorted in the loop
   and the larger stacked. Signed indices: the two walks meet at the ends,
   and `j` below `lo` must end the loop rather than wrap. */
void sort_u32(u32* v, ulong n)
{
    const ulong STACK_MAX = 32;
    long stack_lo[STACK_MAX];
    long stack_hi[STACK_MAX];
    ulong top = 0;
    if (n > 1 && !push_range(stack_lo, stack_hi, top, STACK_MAX, 0, (long)n - 1))
        return;

    while (top != 0)
    {
        top--;
        long lo = stack_lo[top];
        long hi = stack_hi[top];
        while (lo < hi)
        {
            /* A value of the range's own: each walk stops at it at the
               latest, so neither passes the ends */
            u32 pivot = v[lo + (hi - lo) / 2];
            long i = lo;
            long j = hi;
            while (i <= j)
            {
                while (v[i] < pivot)
                    i++;
                while (v[j] > pivot)
                    j--;
                if (i <= j)
                {
                    u32 t = v[i];
                    v[i] = v[j];
                    v[j] = t;
                    i++;
                    j--;
                }
            }
            /* [lo, j] is the smaller side: sorted here, the other stacked */
            if (j - lo < hi - i)
            {
                if (lo < j && !push_range(stack_lo, stack_hi, top, STACK_MAX, lo, j))
                    return;
                lo = i;
            }
            else
            {
                if (i < hi && !push_range(stack_lo, stack_hi, top, STACK_MAX, i, hi))
                    return;
                hi = j;
            }
        }
    }
}

/* A wakeup paired with the first switch to that task after it: what
   `trace latency` walks, over the same window `trace show` prints. */
void latency(stdlib::Printer& con, ulong count, ulong only, const char* const* filter, ulong filter_count)
{
    Window windows[MAX_CPUS] = {};
    ulong total = collect(windows, con, count, only, filter, filter_count);
    ulong cpus = 0;
    for (ulong cpu = 0; cpu < MAX_CPUS; cpu++)
    {
        if (windows[cpu].count != 0)
            cpus++;
    }
    u64 now_cycles;
    ulong now_ns;
    now_point(now_cycles, now_ns);

    /* A task woken and not yet switched to in the window: its wakeup's
       cycles, and the CPU it was queued on */
    u64 wake_at[PID_SLOTS] = {};
    u16 wake_cpu[PID_SLOTS] = {};

    auto waits = static_cast<WakeWait*>(mm::alloc((total != 0 ? total : 1) * sizeof(WakeWait), TAG));
    ulong ran = 0;
    ulong wakeups = 0;
    ulong strange = 0;

    ulong skip = (total > count) ? total - count : 0;
    const TraceRing::Record* r;
    ulong taken = 0;
    while (next_record(windows, r))
    {
        if (taken++ < skip)
            continue;

        if (r->id == (u16)Id::SchedWakeup && r->size == sizeof(SchedWakeup))
        {
            SchedWakeup w;
            stdlib::mem_cpy(&w, r->payload, sizeof(w));
            wakeups++;
            if (w.pid >= PID_SLOTS)
                strange++;
            else if (wake_at[w.pid] == 0)
            {
                wake_at[w.pid] = r->cycles;
                wake_cpu[w.pid] = (u16)((w.cpu < MAX_CPUS) ? w.cpu : 0);
            }
        }
        else if (r->id == (u16)Id::SchedSwitch && r->size == sizeof(SchedSwitch) && waits != nullptr)
        {
            SchedSwitch s;
            stdlib::mem_cpy(&s, r->payload, sizeof(s));
            if (s.next_pid < PID_SLOTS && wake_at[s.next_pid] != 0 && r->cycles >= wake_at[s.next_pid])
            {
                u64 delta = cycle_counter_delta_to_ns(r->cycles - wake_at[s.next_pid]);
                WakeWait& ww = waits[ran++];
                ww.ns = (delta > (u64)0xFFFFFFFF) ? 0xFFFFFFFFU : (u32)delta;
                ww.pid = s.next_pid;
                ww.woken_cpu = wake_cpu[s.next_pid];
                ww.ran_cpu = (u16)((r->cpu < MAX_CPUS) ? r->cpu : 0);
                stdlib::mem_set(ww.comm, 0, sizeof(ww.comm));
                stdlib::mem_cpy(ww.comm, s.next_comm, COMM_BYTES);
                wake_at[s.next_pid] = 0;
            }
        }
    }
    release_windows(windows);

    ulong pending = 0;
    for (ulong pid = 0; pid < PID_SLOTS; pid++)
    {
        if (wake_at[pid] != 0)
            pending++;
    }

    con.printf("trace latency: %lu records on %lu cpus -- %lu wakeups, %lu ran in the window, %lu not run in it\n",
        total, cpus, wakeups, ran, pending);
    if (strange != 0)
        con.printf("  %lu wakeups named a pid the table has no room for, and are left out\n", strange);
    if (ran == 0)
    {
        if (total != 0)
            con.printf("  no wakeup of the window was followed by its task's switch -- "
                "`trace on sched_switch,sched_wakeup`, and a window with both in it\n");
        if (waits != nullptr)
            mm::free(waits);
        return;
    }
    if (waits == nullptr)
    {
        con.printf("  no memory to hold %lu waits\n", ran);
        return;
    }

    /* p50, p90, p99 and the most: the waits sorted, so a percentile is one
       index -- anything else needs a histogram's bounds between the numbers
       and the answer */
    auto sorted = static_cast<u32*>(mm::alloc(ran * sizeof(u32), TAG));
    if (sorted == nullptr)
    {
        con.printf("  no memory to sort %lu waits\n", ran);
        mm::free(waits);
        return;
    }
    for (ulong i = 0; i < ran; i++)
        sorted[i] = waits[i].ns;
    sort_u32(sorted, ran);

    char p50[32];
    char p90[32];
    char p99[32];
    char pmax[32];
    wait_text(p50, sizeof(p50), sorted[(ran - 1) * 50 / 100]);
    wait_text(p90, sizeof(p90), sorted[(ran - 1) * 90 / 100]);
    wait_text(p99, sizeof(p99), sorted[(ran - 1) * 99 / 100]);
    wait_text(pmax, sizeof(pmax), sorted[ran - 1]);
    con.printf("  wait p50 %s p90 %s p99 %s max %s\n", p50, p90, p99, pmax);

    /* The five longest, each with the task and the CPUs: what a histogram
       cannot say */
    const ulong WORST = 5;
    ulong worst[WORST] = {};
    ulong worst_count = 0;
    for (ulong i = 0; i < ran; i++)
    {
        ulong at = worst_count;
        while (at > 0 && waits[worst[at - 1]].ns < waits[i].ns)
            at--;
        if (at >= WORST)
            continue;
        ulong move = (worst_count < WORST) ? worst_count : WORST - 1;
        while (move > at)
        {
            worst[move] = worst[move - 1];
            move--;
        }
        worst[at] = i;
        if (worst_count < WORST)
            worst_count++;
    }
    con.printf("  the longest:\n");
    for (ulong i = 0; i < worst_count; i++)
    {
        const WakeWait& ww = waits[worst[i]];
        char text[32];
        wait_text(text, sizeof(text), ww.ns);
        con.printf("    %9s pid %u %-12s cpu %u -> %u\n", text, ww.pid, ww.comm, ww.woken_cpu, ww.ran_cpu);
    }

    mm::free(sorted);
    mm::free(waits);
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
    if (triggered(t))
    {
        char text[32];
        char name[COMM_BYTES + 1] = {};
        stdlib::mem_cpy(name, t.comm, COMM_BYTES);
        wait_text(text, sizeof(text), t.waited_ns);
        con.printf("trace: frozen by the wakeup trigger: pid %u %s waited %s on cpu %u, at %lu.%06lu\n",
            (u32)(t.pid_cpu >> 32), name, text, (u32)t.pid_cpu, t.at_ns / consts::NANO_SECS_IN_SEC,
            (t.at_ns / consts::NANO_SECS_IN_USEC) % USECS_IN_SEC);
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

/* `trace test [n]`: n Test records on this CPU. */
void test_records(stdlib::Printer& con, ulong n)
{
    if (!on(Id::Test))
    {
        con.printf("trace: `trace on test` first\n");
        return;
    }
    for (ulong i = 0; i < n; i++)
    {
        Test t = { i, i * 0x9E3779B97F4A7C15ULL };
        emit(Id::Test, &t, sizeof(t));
    }
    con.printf("trace: %lu test records on cpu %lu\n", n, CpuTable::get_instance().get_current_cpu_id());
}

/* `trace bench`: what a tracepoint costs this CPU off, and on. */
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

    if (have_rings())
    {
        __atomic_store_n(&enabled[(ulong)Id::Test], 1, __ATOMIC_RELEASE);
        start = hal::read_cycle_counter();
        for (ulong i = 0; i < ON_LOOPS; i++)
        {
            Test t = { i, i };
            emit(Id::Test, &t, sizeof(t));
        }
        u64 on_cycles = hal::read_cycle_counter() - start;
        ulong on_ps = cycle_counter_delta_to_ns(on_cycles) * 1000 / ON_LOOPS;
        con.printf("trace bench: one that is on, %lu.%03lu ns a record (%lu records into this cpu's ring)\n",
            on_ps / 1000, on_ps % 1000, ON_LOOPS);
    }
    else
    {
        con.printf("trace bench: no rings -- `trace on test` first to time one that is on\n");
    }
    __atomic_store_n(&enabled[(ulong)Id::Test], was_on ? 1 : 0, __ATOMIC_RELEASE);
    (void)hits;
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

    ulong flags = hal::irq_save();
    ulong cpu = hal::get_current_cpu_hw_id();
    if (cpu < MAX_CPUS)
    {
        TraceRing* ring = __atomic_load_n(&rings[cpu], __ATOMIC_ACQUIRE);
        if (ring != nullptr)
            ring->write((u16)n, hal::read_cycle_counter(), payload, size);
    }
    hal::irq_restore(flags);
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
       goes with them. */
    if (have_rings() && !make_rings(con))
    {
        __atomic_store_n(&ring_kib, was, __ATOMIC_RELAXED);
        return false;
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

    bool chosen[COUNT] = {};
    long named = 0;
    for (ulong w = 0; w < n; w++)
    {
        bool found = false;
        for (ulong i = 0; i < COUNT; i++)
        {
            if (stdlib::str_cmp(words[w], "all") == 0 || stdlib::str_cmp(words[w], EVENTS[i].name) == 0
                || stdlib::str_cmp(words[w], EVENTS[i].group) == 0)
            {
                chosen[i] = true;
                found = true;
            }
        }
        if (!found)
            con.printf("trace: no event or group called %s\n", words[w]);
    }
    for (ulong i = 0; i < COUNT; i++)
        named += chosen[i] ? 1 : 0;
    if (named == 0)
        return -1;

    stdlib::AutoLock lock(control());
    if (turn_on && !have_rings() && !make_rings(con))
        return -1;
    for (ulong i = 0; i < COUNT; i++)
    {
        if (chosen[i])
            __atomic_store_n(&enabled[i], turn_on ? 1 : 0, __ATOMIC_RELEASE);
    }
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

/* show's, dump's and latency's arguments: [n] [cpu=N] [event...]. */
struct WindowArgs
{
    ulong count;
    ulong only;
    const char* filter[WORDS_MAX];
    ulong filter_count;
};

void window_args(char words[WORDS_MAX][WORD_LEN], ulong n, WindowArgs& out)
{
    out.count = SHOW_DEFAULT;
    out.only = MAX_CPUS;
    out.filter_count = 0;
    for (ulong w = 1; w < n; w++)
    {
        ulong value;
        if (stdlib::mem_cmp(words[w], "cpu=", 4) == 0 && stdlib::parse_ulong(words[w] + 4, value) && value < MAX_CPUS)
            out.only = value;
        else if (stdlib::parse_ulong(words[w], value) && value != 0)
            out.count = (value < SHOW_MAX) ? value : SHOW_MAX;
        else if (out.filter_count < WORDS_MAX)
            out.filter[out.filter_count++] = words[w];
    }
}

/* `wakeup>N` and a unit -- ns, us, ms or s -- as `trace freeze` takes it. */
bool parse_wakeup_trigger(const char* word, u64& ns)
{
    const ulong PREFIX_LEN = 7;
    if (stdlib::mem_cmp(word, "wakeup>", PREFIX_LEN) != 0)
        return false;

    const char* p = word + PREFIX_LEN;
    u64 value = 0;
    ulong digits = 0;
    while (*p >= '0' && *p <= '9')
    {
        value = value * 10 + (u64)(*p - '0');
        /* A billion of any unit is more than this measures, and keeps the
           scale below from wrapping */
        if (value > 1000000000ULL)
            return false;
        p++;
        digits++;
    }
    if (digits == 0)
        return false;

    u64 scale;
    if (stdlib::str_cmp(p, "ns") == 0)
        scale = 1;
    else if (stdlib::str_cmp(p, "us") == 0)
        scale = consts::NANO_SECS_IN_USEC;
    else if (stdlib::str_cmp(p, "ms") == 0)
        scale = consts::NANO_SECS_IN_MS;
    else if (stdlib::str_cmp(p, "s") == 0)
        scale = consts::NANO_SECS_IN_SEC;
    else
        return false;

    ns = value * scale;
    return ns != 0;
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
        WindowArgs window;
        window_args(words, n, window);
        stdlib::AutoLock lock(control());
        if (stdlib::str_cmp(verb, "show") == 0)
            show(con, window.count, window.only, window.filter, window.filter_count);
        else if (stdlib::str_cmp(verb, "dump") == 0)
            dump(con, window.count, window.only, window.filter, window.filter_count);
        else
            latency(con, window.count, window.only, window.filter, window.filter_count);
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

            /* Armed, and recording: a frozen ring has nothing for a trigger
               to keep */
            stdlib::AutoLock lock(control());
            disarm_trigger();
            __atomic_store_n(&frozen, 0, __ATOMIC_RELEASE);
            __atomic_store_n(&wakeup_trigger_ns, over, __ATOMIC_RELEASE);
            char text[32];
            wait_text(text, sizeof(text), over);
            con.printf("trace: armed -- the rings freeze when a wakeup waits over %s\n", text);
            return;
        }

        stdlib::AutoLock lock(control());
        disarm_trigger();
        __atomic_store_n(&frozen, freeze ? 1 : 0, __ATOMIC_RELEASE);
        con.printf("trace: %s\n", freeze ? "frozen -- nothing more is recorded until `trace thaw`" : "recording");
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
        con.printf("usage: trace [list] | on <event|group|all>... | off <...> | show [n] [cpu=N] [event...] | "
            "dump [n] [cpu=N] [event...] | latency [n] [cpu=N] [event...] | stat | clear | size <KiB> | free | "
            "freeze [wakeup>N<ns|us|ms|s>] | thaw | test [n] | bench\n");
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

void emit_sched_wakeup(Task* task, ulong cpu, bool deadline, Task* waker)
{
    SchedWakeup w = {};
    w.pid = task_pid(task);
    put_comm(w.comm, task_name(task));
    w.cpu = (u32)cpu;
    w.deadline = deadline ? 1 : 0;
    w.waker_pid = task_pid(waker);
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
