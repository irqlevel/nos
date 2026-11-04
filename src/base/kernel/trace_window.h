#pragma once

#include <lib/stdlib.h>
#include <kernel/trace_ring.h>
#include <kernel/tracepoint.h>

namespace kernel
{
namespace tp
{

/* What the shell's readers of the rings make of them -- the window
   `trace show`, `trace dump` and `trace latency` read, and the waits
   latency pairs out of it -- with nothing in it of the kernel but a ring and
   the events' payloads: the host's fuzzer (`cpp-fuzz.py --target
   tracewin`) runs it as the kernel does, and holds it to a model. */

/* A set of events, a bit each. */
using EventMask = u64;
static_assert(COUNT <= 64, "an event mask holds every event");
const EventMask ALL_EVENTS = (COUNT == 64) ? ~(EventMask)0 : (((EventMask)1 << COUNT) - 1);

inline EventMask event_bit(ulong id)
{
    return (id < COUNT) ? ((EventMask)1 << id) : 0;
}

/* complete_from of a CPU's part of a window of which nothing could be read:
   not even its newest record. */
const u64 COMPLETE_NEVER = ~(u64)0;

/* One CPU's part of a window: its records, oldest first, and from when on
   none of the window's events that CPU wrote is missing from them. */
struct Window
{
    TraceRing::Record* records;
    ulong count;
    /* merge_next()'s place in it */
    ulong next;
    /* The cycle count from which on every record of the window's events
       the CPU wrote is among `records`: 0 when nothing since the window's
       start was lost -- written over by the ring, or left out for room. */
    u64 complete_from;
};

/* Ring `ring`'s newest `want` records of the events in `mask`, of those
   written from index `from` on (a `trace clear`'s, or 0), into `out` --
   room for `want` -- oldest first, each marked as `cpu`'s: how many. The
   ring is read from its newest record back while its CPU writes on: a
   record written over as it is read ends the walk, every older one being
   gone too. `complete_from` says from where on the window lacks none of
   `mask`'s records (Window::complete_from). With every event in `mask`,
   records of an event this kernel does not know are kept too. */
ulong read_window(const TraceRing& ring, u64 from, ulong want, EventMask mask, u32 cpu, TraceRing::Record* out,
    u64& complete_from);

/* The next record of `windows` (`count` of them) by cycle count, oldest
   first, the first window's of two as old: false when they are spent. */
bool merge_next(Window* windows, ulong count, const TraceRing::Record*& out);

/* From where on no part of the window lacks a record: the latest
   complete_from of `windows`, with `cpu` the window it is whose -- `count`
   when nothing of any is missing. */
u64 window_start(const Window* windows, ulong count, ulong& cpu);

/* A wait: a wakeup, and the first switch to its task after it. */
struct WakeWait
{
    /* From the wakeup -- a deadline's from the deadline itself -- to the
       switch, in ns: clamped to what 32 bits hold */
    u32 ns;
    u32 pid;
    /* The queue it was woken onto, and the CPU that switched to it */
    u16 woken_cpu;
    u16 ran_cpu;
    char comm[COMM_BYTES + 1];
};

/* `trace latency`'s pairing: each wakeup kept by its task's pid until a
   switch to that task answers it. Fed a window's records oldest first.
   A record older than `start` -- from before every CPU's part of the window
   is whole (window_start) -- is counted and left out: a wakeup from there
   could have its switch among what one CPU's ring lost, and be paired with
   a later switch to its task instead. */
class WakePairing
{
public:
    /* Task ids are ObjectTable's: 0 to 255. */
    static const ulong PID_SLOTS = 256;

    /* `waits` with room for `room` of them; `to_ns` turns a difference of
       cycle counts into ns, and is not null. */
    WakePairing(WakeWait* waits, ulong room, u64 start, ulong (*to_ns)(u64 delta));

    void feed(const TraceRing::Record& r);

    /* What was paired, in `waits` in the order the switches came */
    ulong waits() const { return ran_; }
    /* The wakeups fed: of a pid the table has no room for too */
    ulong wakeups() const { return wakeups_; }
    /* Wakeups no switch to their task answered: still waiting, or switched
       to after the window */
    ulong pending() const;
    /* Wakeups of a task woken again before any switch to it: the switch
       after the first is not in the window */
    ulong superseded() const { return superseded_; }
    /* Wakeups of a pid past PID_SLOTS */
    ulong strange() const { return strange_; }
    /* Records left out for being older than `start` */
    ulong before_start() const { return before_start_; }
    /* Waits paired with no room left for them */
    ulong dropped() const { return dropped_; }

private:
    WakePairing(const WakePairing& other) = delete;
    WakePairing& operator=(const WakePairing& other) = delete;

    WakeWait* waits_;
    ulong room_;
    u64 start_;
    ulong (*to_ns_)(u64 delta);
    ulong ran_;
    ulong wakeups_;
    ulong superseded_;
    ulong strange_;
    ulong before_start_;
    ulong dropped_;
    /* A task's wakeup not yet answered: its cycle count, how late a
       deadline's ran past the deadline, the queue it went to */
    u64 wake_at_[PID_SLOTS];
    u32 late_ns_[PID_SLOTS];
    u16 wake_cpu_[PID_SLOTS];
    u8 pending_[PID_SLOTS];
};

/* `v`, smallest first. A heapsort: in place, with no stack of its own, and
   n log n whatever the order -- a quicksort's stack, pushed the wrong way
   round, was a panic on waits that rose and fell. */
void sort_u32(u32* v, ulong n);

/* The indices of `waits`' `k` longest into `out` (room for `k`), longest
   first and of two as long the earlier: how many -- `k`, or `n` when there
   are fewer. */
ulong top_waits(const WakeWait* waits, ulong n, ulong* out, ulong k);

/* The index of the p-th percentile (0 to 100) of n values sorted: the lower
   of the two around it; 0 for none. */
ulong percentile_index(ulong n, ulong p);

/* A duration as `trace freeze wakeup>` takes it: digits and a unit -- ns,
   us, ms or s -- more than 0 and at most a billion of the unit: false for
   anything else. */
bool parse_duration(const char* text, u64& ns);

}
}
