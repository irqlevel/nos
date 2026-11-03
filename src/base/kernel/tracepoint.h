#pragma once

#include <lib/stdlib.h>
#include <lib/printer.h>
#include <kernel/trace_ring.h>

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

}
}
