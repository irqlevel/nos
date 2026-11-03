// What the shell's readers make of the tracepoints' rings (kernel/trace_window.cpp):
// the window `trace show`, `trace dump` and `trace latency` read and merge,
// and the waits latency pairs out of it -- run as the kernel runs them, on
// rings of the kernel's own (kernel/trace_ring.cpp).
//
// A schedule is made up and played out on a few CPUs -- tasks woken by one
// CPU onto another's queue, some at a deadline and late, switched to,
// displaced, switched to again, blocked; idle and IPI records between --
// each CPU's records into its ring, which wraps, and a `trace clear` now and
// then. What the schedule did is known: every wakeup and the first switch
// to its task after it. Then the window is read as `trace latency` reads
// it, with a window size and an event filter of the input's, and held to
// that truth: each CPU's part the newest records of its ring that the
// filter keeps, every one as written; nothing the CPU wrote missing from
// its part from where the part says it is whole; the merge the newest of
// them all in time order; and every wait paired one the schedule had --
// its task, its CPUs, from its wakeup (a deadline's, from the deadline) to
// the first switch to it -- with none it had left out, and no wakeup ever
// found waking a task twice. A wait made of a wakeup and a switch that was
// not its own -- the one past a switch a ring lost -- is a finding, as is
// a panic: the quicksort this replaced pushed the wrong half and overflowed
// its stack on waits that rose and fell.
//
// Then the sort against std::sort on orders made to be its worst, the five
// longest against a stable sort, a duration's parse against a model of its
// own, and a writer thread filling a ring while its window is read.
#include "host.h"

#include <kernel/trace_ring.h>
#include <kernel/trace_window.h>

#include "fuzz.h"
#include "kernel.h"

#include <string.h>

#include <algorithm>
#include <atomic>
#include <string>
#include <thread>
#include <vector>

namespace
{

using kernel::TraceRing;
namespace tp = kernel::tp;

const ulong TAG = 0x77546E57;

/* The kernel's conversion is the HAL's; here a cycle is a nanosecond. */
ulong identity_ns(u64 delta)
{
    return (ulong)delta;
}

/* The bulk of a schedule's choices, from a seed the input gives: an input
   is a few dozen choices, a schedule thousands. */
class Prng
{
public:
    explicit Prng(u64 seed)
        : s_(seed ? seed : 0x9E3779B97F4A7C15ULL)
    {
    }

    u64 next()
    {
        s_ ^= s_ << 13;
        s_ ^= s_ >> 7;
        s_ ^= s_ << 17;
        return s_;
    }

    u64 below(u64 n) { return (n == 0) ? 0 : next() % n; }
    bool chance(u64 one_in) { return below(one_in) == 0; }

private:
    u64 s_;
};

/* ---- the sort, the five longest, the duration ---- */

void run_sort(fuzz::Input& in)
{
    ulong n = in.chance(32) ? in.range(4097, 30000) : in.below(4097);
    Prng r(in.u64());
    std::vector<u32> v(n);
    ulong shape = in.below(8);
    ulong mod = in.range(1, 64);
    for (ulong i = 0; i < n; i++)
    {
        switch (shape)
        {
        case 0:
            v[i] = (u32)r.next();
            break;
        case 1:
            v[i] = (u32)i;
            break;
        case 2:
            v[i] = (u32)(n - i);
            break;
        case 3:
            /* Rising, then falling: what a burst of load does to waits, and
               what overflowed the quicksort's stack */
            v[i] = (u32)((i < n / 2) ? i : n - i);
            break;
        case 4:
            v[i] = 7;
            break;
        case 5:
            v[i] = (u32)r.below(4);
            break;
        case 6:
            v[i] = (u32)(i % mod);
            break;
        default:
            /* Falling, then rising */
            v[i] = (u32)((i < n / 2) ? n / 2 - i : i - n / 2);
            break;
        }
    }
    /* Now and then a few disturbed */
    if (n != 0 && in.chance(64))
    {
        for (ulong k = 0; k < n / 20 + 1; k++)
            v[r.below(n)] = (u32)r.next();
    }

    std::vector<u32> want = v;
    std::sort(want.begin(), want.end());
    tp::sort_u32(v.data(), n);
    INVARIANT(v == want, "sort_u32 of %lu values of shape %lu is not std::sort's", n, shape);
    fuzz::reached(shape == 3 ? "rising and falling sorted" : "values sorted");
}

void run_top(fuzz::Input& in)
{
    ulong n = in.below(300);
    ulong k = in.below(9);
    Prng r(in.u64());
    u32 spread = in.boolean() ? 4 : 0xFFFFFFFF;
    std::vector<tp::WakeWait> waits(n);
    for (ulong i = 0; i < n; i++)
    {
        waits[i] = {};
        waits[i].ns = (u32)(r.next() % ((u64)spread + 1));
        waits[i].pid = (u32)i;
    }

    std::vector<ulong> out(k + 1, 0xDEAD);
    ulong got = tp::top_waits(waits.data(), n, out.data(), k);

    std::vector<ulong> order(n);
    for (ulong i = 0; i < n; i++)
        order[i] = i;
    std::stable_sort(order.begin(), order.end(),
        [&](ulong a, ulong b) { return waits[a].ns > waits[b].ns; });
    ulong expect = std::min(k, n);
    INVARIANT(got == expect, "top_waits gave %lu of %lu for k %lu", got, n, k);
    for (ulong i = 0; i < got; i++)
        INVARIANT(out[i] == order[i], "top_waits' %luth is wait %lu, not %lu", i, out[i], order[i]);
    INVARIANT(out[k] == 0xDEAD, "top_waits wrote past its k");

    /* The percentile's index: in the array, and not going down as p goes up */
    ulong m = in.below(5000);
    ulong last = 0;
    for (ulong p = 0; p <= 101; p++)
    {
        ulong at = tp::percentile_index(m, p);
        INVARIANT(m == 0 ? at == 0 : at < m, "percentile %lu of %lu is index %lu", p, m, at);
        INVARIANT(at >= last, "percentile %lu of %lu went down", p, m);
        last = at;
    }
    if (got != 0)
        fuzz::reached("the longest waits");
}

/* What parse_duration should say: the digits as a number of any length,
   then the unit, the whole rest of the word. */
bool model_duration(const std::string& s, u64& ns)
{
    size_t i = 0;
    unsigned __int128 value = 0;
    bool big = false;
    while (i < s.size() && s[i] >= '0' && s[i] <= '9')
    {
        value = value * 10 + (unsigned)(s[i] - '0');
        if (value > 1000000000ULL)
            big = true;
        i++;
    }
    if (i == 0 || big)
        return false;
    std::string unit = s.substr(i);
    u64 scale;
    if (unit == "ns")
        scale = 1;
    else if (unit == "us")
        scale = 1000;
    else if (unit == "ms")
        scale = 1000000;
    else if (unit == "s")
        scale = 1000000000;
    else
        return false;
    ns = (u64)value * scale;
    return ns != 0;
}

void run_duration(fuzz::Input& in)
{
    static const char* const UNITS[] = {"ns", "us", "ms", "s", "", "sec", "m", "us ", "uS", "nss", "ms0"};
    std::string s;
    if (in.chance(16))
        s += (char)in.range(1, 127);
    ulong digits = in.below(14);
    for (ulong i = 0; i < digits; i++)
        s += (char)('0' + in.below(10));
    s += in.pick(UNITS);
    if (in.chance(16))
        s += (char)in.range(1, 127);

    u64 got = 0xABCD, want = 0;
    bool ok = tp::parse_duration(s.c_str(), got);
    bool want_ok = model_duration(s, want);
    INVARIANT(ok == want_ok && (!ok || got == want), "parse_duration(\"%s\") said %d %lu, not %d %lu", s.c_str(), ok,
              (ulong)got, want_ok, (ulong)want);
    if (ok)
        fuzz::reached("a duration parsed");
}

/* ---- a schedule, its rings, and the window read of them ---- */

const ulong MAX_SIM_CPUS = 6;
/* Pids past the pairing's table are its strange ones; the CPUs' idle
   tasks have pids nothing wakes */
const u32 IDLE_PID_BASE = 240;

struct Written
{
    u64 index;
    u64 cycles;
    u16 id;
    u16 size;
    uint8_t payload[TraceRing::PAYLOAD_BYTES];
};

/* A wakeup and the first switch to its task after it, as the schedule had
   them. */
struct Truth
{
    u32 pid;
    u64 wake_cycles;
    u32 late;
    u16 woken_cpu;
    ulong wake_cpu;
    u64 switch_cycles;
    u16 ran_cpu;
};

enum class St
{
    Sleeping,
    Woken,
    Running,
    Runnable,
};

struct SimTask
{
    u32 pid;
    St st;
    ulong cpu;
    /* While Woken: its truth, waiting for the switch */
    long truth;
};

struct Sim
{
    ulong cpus;
    TraceRing rings[MAX_SIM_CPUS];
    std::vector<Written> written[MAX_SIM_CPUS];
    u64 cleared[MAX_SIM_CPUS];
    /* Who runs on each CPU: a task's index, or -1 for its idle task */
    long running[MAX_SIM_CPUS];
    std::vector<SimTask> tasks;
    std::vector<Truth> truth;
    u64 now;
};

void comm_of(u32 pid, char* to)
{
    memset(to, 0, tp::COMM_BYTES);
    snprintf(to, tp::COMM_BYTES, "t%u", pid);
}

void put(Sim& sim, ulong cpu, tp::Id id, const void* payload, ulong size)
{
    sim.now += 1 + (sim.now % 7);
    Written w = {};
    w.index = sim.written[cpu].size();
    w.cycles = sim.now;
    w.id = (u16)id;
    w.size = (u16)size;
    memcpy(w.payload, payload, size);
    INVARIANT(sim.rings[cpu].write(w.id, w.cycles, payload, size), "a write of %lu bytes refused", size);
    sim.written[cpu].push_back(w);
}

u32 pid_running(const Sim& sim, ulong cpu)
{
    long t = sim.running[cpu];
    return (t < 0) ? IDLE_PID_BASE + (u32)cpu : sim.tasks[t].pid;
}

/* `cpu` switches from whoever runs there to task `next` (-1: its idle task),
   the one before going to be `prev_state`. */
void do_switch(Sim& sim, ulong cpu, long next, tp::SwitchState prev_state)
{
    long prev = sim.running[cpu];
    tp::SchedSwitch s = {};
    s.prev_pid = pid_running(sim, cpu);
    comm_of(s.prev_pid, s.prev_comm);
    s.next_pid = (next < 0) ? IDLE_PID_BASE + (u32)cpu : sim.tasks[next].pid;
    comm_of(s.next_pid, s.next_comm);
    s.next_cpu = (u32)cpu;
    s.prev_state = (u8)prev_state;
    s.reason = 0;
    put(sim, cpu, tp::Id::SchedSwitch, &s, sizeof(s));

    if (prev >= 0)
    {
        SimTask& p = sim.tasks[prev];
        p.st = (prev_state == tp::SwitchState::Runnable) ? St::Runnable : St::Sleeping;
    }
    sim.running[cpu] = next;
    if (next >= 0)
    {
        SimTask& n = sim.tasks[next];
        if (n.st == St::Woken)
        {
            Truth& t = sim.truth[n.truth];
            t.switch_cycles = sim.now;
            t.ran_cpu = (u16)cpu;
        }
        n.st = St::Running;
        n.cpu = cpu;
        n.truth = -1;
    }
}

void noise(Sim& sim, ulong cpu, Prng& r)
{
    switch (r.below(3))
    {
    case 0:
    case 1:
    {
        tp::Idle i = {(u32)cpu, IDLE_PID_BASE + (u32)cpu};
        put(sim, cpu, r.chance(2) ? tp::Id::IdleEnter : tp::Id::IdleExit, &i, sizeof(i));
        break;
    }
    default:
    {
        tp::SchedIpi i = {(u32)cpu, (u32)r.below(3)};
        put(sim, cpu, tp::Id::SchedIpi, &i, sizeof(i));
        break;
    }
    }
}

void play(Sim& sim, fuzz::Input& in)
{
    Prng r(in.u64());
    ulong steps = in.range(1, 6000);
    ulong noise_one_in = in.range(1, 8);
    bool clear_once = in.chance(64);
    ulong clear_at = r.below(steps);

    for (ulong step = 0; step < steps; step++)
    {
        if (clear_once && step == clear_at)
        {
            for (ulong c = 0; c < sim.cpus; c++)
                sim.cleared[c] = sim.rings[c].head();
            fuzz::reached("a trace clear");
        }

        ulong cpu = r.below(sim.cpus);
        if (r.chance(noise_one_in))
        {
            noise(sim, cpu, r);
            continue;
        }

        long t = (long)r.below(sim.tasks.size());
        SimTask& task = sim.tasks[t];
        switch (task.st)
        {
        case St::Sleeping:
        {
            /* Woken by `cpu` onto a queue of the schedule's choosing, now
               and then by its deadline, late */
            tp::SchedWakeup w = {};
            w.pid = task.pid;
            comm_of(w.pid, w.comm);
            w.cpu = (u32)r.below(sim.cpus);
            w.deadline = r.chance(3) ? 1 : 0;
            w.waker_pid = pid_running(sim, cpu);
            w.late_ns = w.deadline ? (u32)r.below(5000) : 0;
            put(sim, cpu, tp::Id::SchedWakeup, &w, sizeof(w));
            task.st = St::Woken;
            task.truth = (long)sim.truth.size();
            sim.truth.push_back({task.pid, sim.now, w.late_ns, (u16)w.cpu, cpu, 0, 0});
            break;
        }
        case St::Woken:
        case St::Runnable:
        {
            /* Switched to on a CPU: whoever runs there displaced, or the
               CPU's idle task left */
            ulong on = r.below(sim.cpus);
            do_switch(sim, on, t, tp::SwitchState::Runnable);
            break;
        }
        case St::Running:
        {
            if (r.chance(2))
            {
                /* Blocks: its CPU to its idle task, or to another that can run */
                long next = -1;
                for (long i = 0; i < (long)sim.tasks.size(); i++)
                {
                    if (i != t && (sim.tasks[i].st == St::Woken || sim.tasks[i].st == St::Runnable) && r.chance(3))
                    {
                        next = i;
                        break;
                    }
                }
                do_switch(sim, task.cpu, next, r.chance(2) ? tp::SwitchState::Blocked : tp::SwitchState::Sleeping);
            }
            else
            {
                /* Displaced, still runnable: to its CPU's idle task */
                do_switch(sim, task.cpu, -1, tp::SwitchState::Runnable);
            }
            break;
        }
        }
    }
}

/* What the window's part of `cpu` must be: the newest `want` records the
   ring still holds of those written since the clear, that `mask` keeps. */
std::vector<const Written*> model_part(const Sim& sim, ulong cpu, ulong want, tp::EventMask mask)
{
    const auto& w = sim.written[cpu];
    u64 head = w.size();
    u64 cap = sim.rings[cpu].capacity();
    u64 from = sim.cleared[cpu];
    if (head > cap && head - cap > from)
        from = head - cap;
    std::vector<const Written*> part;
    for (u64 i = head; i > from && part.size() < want; i--)
    {
        const Written& x = w[i - 1];
        if (mask == tp::ALL_EVENTS || (tp::event_bit(x.id) & mask) != 0)
            part.push_back(&x);
    }
    std::reverse(part.begin(), part.end());
    return part;
}

void run_window(fuzz::Input& in)
{
    static const ulong CHUNKS[] = {1, 1, 2};
    const size_t blocks = fuzz::heap_blocks();

    Sim sim;
    sim.cpus = in.range(1, MAX_SIM_CPUS);
    sim.now = in.below(1000);
    for (ulong c = 0; c < MAX_SIM_CPUS; c++)
    {
        sim.cleared[c] = 0;
        sim.running[c] = -1;
    }
    for (ulong c = 0; c < sim.cpus; c++)
        INVARIANT(sim.rings[c].init(in.pick(CHUNKS), TAG), "a ring of a chunk or two not made");

    ulong ntasks = in.range(1, 30);
    for (ulong i = 0; i < ntasks; i++)
    {
        /* Mostly pids the pairing's table holds; now and then one past it */
        u32 pid = in.chance(16) ? (u32)(tp::WakePairing::PID_SLOTS + i) : (u32)i;
        sim.tasks.push_back({pid, St::Sleeping, 0, -1});
    }
    play(sim, in);

    /* The window, as collect() reads it */
    static const tp::EventMask WAITS = tp::event_bit((ulong)tp::Id::SchedWakeup) |
                                       tp::event_bit((ulong)tp::Id::SchedSwitch);
    tp::EventMask masks[] = {tp::ALL_EVENTS, WAITS,
                             WAITS | tp::event_bit((ulong)tp::Id::IdleEnter) |
                                 tp::event_bit((ulong)tp::Id::IdleExit),
                             tp::event_bit((ulong)tp::Id::SchedWakeup)};
    tp::EventMask mask = in.pick(masks);
    ulong count = in.chance(64) ? 4096 : in.range(1, 3000);

    tp::Window windows[MAX_SIM_CPUS] = {};
    std::vector<std::vector<TraceRing::Record>> storage(sim.cpus);
    ulong total = 0;
    for (ulong c = 0; c < sim.cpus; c++)
    {
        const TraceRing& ring = sim.rings[c];
        u64 head = ring.head();
        if (head <= sim.cleared[c])
            continue;
        ulong want = std::min<u64>(std::min<u64>(count, ring.capacity()), head - sim.cleared[c]);
        storage[c].resize(want);
        u64 complete;
        ulong kept = tp::read_window(ring, sim.cleared[c], want, mask, (u32)c, storage[c].data(), complete);
        windows[c] = {storage[c].data(), kept, 0, complete};
        total += kept;

        /* The part: the model's, record for record */
        auto model = model_part(sim, c, want, mask);
        INVARIANT(kept == model.size(), "cpu %lu's part holds %lu records, the model's %zu (want %lu)", c, kept,
                  model.size(), want);
        for (ulong i = 0; i < kept; i++)
        {
            const TraceRing::Record& got = storage[c][i];
            const Written& m = *model[i];
            INVARIANT(got.index == m.index && got.cycles == m.cycles && got.id == m.id && got.size == m.size &&
                          got.cpu == c && memcmp(got.payload, m.payload, m.size) == 0,
                      "cpu %lu's part's record %lu is index %lu at %lu, the model's index %lu at %lu", c, i,
                      (ulong)got.index, (ulong)got.cycles, (ulong)m.index, (ulong)m.cycles);
        }

        /* Whole from where it says: every record the filter keeps written on
           this CPU since the clear, from complete_from on, is in it */
        INVARIANT(complete != tp::COMPLETE_NEVER, "cpu %lu's part: nothing read, the ring still", c);
        ulong in_part = 0;
        for (u64 i = sim.cleared[c]; i < sim.written[c].size(); i++)
        {
            const Written& x = sim.written[c][i];
            if (mask != tp::ALL_EVENTS && (tp::event_bit(x.id) & mask) == 0)
                continue;
            if (x.cycles < complete)
                continue;
            in_part++;
        }
        INVARIANT(in_part == kept, "cpu %lu's part says it is whole from %lu, with %lu records of %lu from there", c,
                  (ulong)complete, kept, in_part);
        if (complete != 0)
            fuzz::reached(head - sim.cleared[c] > ring.capacity() ? "a part the ring wrapped" : "a part cut for room");
    }

    /* The merge: the newest of them all, oldest first */
    ulong walked = std::min(total, count);
    ulong skip = total - walked;
    ulong limit_cpu;
    u64 start = tp::window_start(windows, sim.cpus, limit_cpu);
    ulong room = in.chance(16) ? in.below(walked + 1) : walked;
    std::vector<tp::WakeWait> waits(room + 1);
    tp::WakePairing pairing(waits.data(), room, start, identity_ns);

    std::vector<const TraceRing::Record*> fed;
    const TraceRing::Record* r;
    ulong taken = 0;
    u64 last = 0;
    while (tp::merge_next(windows, sim.cpus, r))
    {
        INVARIANT(r->cycles >= last, "the merge went back in time: %lu after %lu", (ulong)r->cycles, (ulong)last);
        last = r->cycles;
        if (taken++ < skip)
            continue;
        fed.push_back(r);
        pairing.feed(*r);
    }
    INVARIANT(taken == total, "the merge gave %lu of %lu records", taken, total);

    /* What the truth says the pairing finds: every wakeup fed from `start`
       on whose first switch is fed too, in the order the switches came */
    std::vector<std::pair<u64, ulong>> fed_at;
    for (const TraceRing::Record* x : fed)
        fed_at.push_back({x->cycles, x->cpu});
    auto was_fed = [&](u64 cycles, ulong cpu) {
        return std::binary_search(fed_at.begin(), fed_at.end(), std::make_pair(cycles, cpu));
    };
    std::sort(fed_at.begin(), fed_at.end());

    /* Per task, its wakeups fed from `start` on, in order: each answered by
       its first switch if that was fed; one whose switch was not, followed
       by another of the task's, superseded by it; the last, pending */
    std::vector<const Truth*> expect;
    ulong fed_wakeups = 0, fed_strange = 0, pending = 0, superseded = 0;
    std::vector<std::vector<const Truth*>> by_pid(tp::WakePairing::PID_SLOTS);
    for (const Truth& t : sim.truth)
    {
        if (!was_fed(t.wake_cycles, t.wake_cpu) || t.wake_cycles < start)
            continue;
        fed_wakeups++;
        if (t.pid >= tp::WakePairing::PID_SLOTS)
        {
            fed_strange++;
            continue;
        }
        by_pid[t.pid].push_back(&t);
    }
    for (const auto& list : by_pid)
    {
        for (size_t i = 0; i < list.size(); i++)
        {
            const Truth& t = *list[i];
            if (t.switch_cycles != 0 && was_fed(t.switch_cycles, t.ran_cpu))
                expect.push_back(&t);
            else if (i + 1 < list.size())
                superseded++;
            else
                pending++;
        }
    }
    std::sort(expect.begin(), expect.end(),
              [](const Truth* a, const Truth* b) { return a->switch_cycles < b->switch_cycles; });

    ulong before = 0;
    for (const TraceRing::Record* x : fed)
        before += (x->cycles < start) ? 1 : 0;
    INVARIANT(pairing.before_start() == before, "%lu records before the start counted, %lu fed", pairing.before_start(),
              before);
    INVARIANT(pairing.wakeups() == fed_wakeups && pairing.strange() == fed_strange,
              "%lu wakeups (%lu strange) counted, the truth %lu (%lu)", pairing.wakeups(), pairing.strange(),
              fed_wakeups, fed_strange);
    INVARIANT(pairing.superseded() == superseded, "%lu wakeups superseded, the truth %lu", pairing.superseded(),
              superseded);
    /* With switches in the window, one whole from `start` lost none: a task
       is woken again only after a switch to it */
    if ((mask & tp::event_bit((ulong)tp::Id::SchedSwitch)) != 0)
        INVARIANT(superseded == 0, "a task woken twice with no switch between, in a window whole from %lu",
                  (ulong)start);

    ulong paired = std::min<ulong>(expect.size(), room);
    INVARIANT(pairing.waits() == paired && pairing.dropped() == expect.size() - paired,
              "%lu waits paired and %lu dropped, the truth %zu with room for %lu", pairing.waits(),
              pairing.dropped(), expect.size(), room);
    /* A switch with no room for its wait still answers its wakeup */
    INVARIANT(pairing.pending() == pending, "%lu wakeups pending, the truth %lu", pairing.pending(), pending);
    for (ulong i = 0; i < paired; i++)
    {
        const Truth& t = *expect[i];
        const tp::WakeWait& w = waits[i];
        u64 ns = t.switch_cycles - t.wake_cycles + t.late;
        u32 want_ns = (u32)std::min<u64>(ns, 0xFFFFFFFFULL);
        char comm[tp::COMM_BYTES + 1] = {};
        comm_of(t.pid, comm);
        INVARIANT(w.pid == t.pid && w.ns == want_ns && w.woken_cpu == t.woken_cpu && w.ran_cpu == t.ran_cpu &&
                      strcmp(w.comm, comm) == 0,
                  "wait %lu is pid %u %u ns from cpu %u to %u (%s), the truth pid %u %u ns from %u to %u", i, w.pid,
                  w.ns, w.woken_cpu, w.ran_cpu, w.comm, t.pid, want_ns, t.woken_cpu, t.ran_cpu);
    }
    if (paired != 0)
        fuzz::reached(start != 0 ? "waits paired from a cut window" : "waits paired");
    if (pairing.dropped() != 0)
        fuzz::reached("waits with no room");

    for (ulong c = 0; c < sim.cpus; c++)
        sim.rings[c].release();
    storage.clear();
    INVARIANT(fuzz::heap_blocks() == blocks, "the rings kept %zu blocks", fuzz::heap_blocks() - blocks);
}

/* A writer thread on a ring and its window read as it writes: every record
   whole and the one its index says, the part's records consecutive and the
   newest there were when it was read, and whole from where it says. */
void run_threads(fuzz::Input& in)
{
    const size_t blocks = fuzz::heap_blocks();
    TraceRing ring;
    INVARIANT(ring.init(1, TAG), "a ring of a chunk not made");
    const u64 cap = ring.capacity();
    const u64 limit = in.range(cap, cap * 64);
    const ulong want = in.range(1, cap);
    fuzz::reached("a window read as its ring is written");

    std::atomic<bool> stop{false};
    std::thread writer([&] {
        for (u64 i = 0; i < limit && !stop.load(std::memory_order_relaxed);)
        {
            u64 p[2] = {i, i ^ 0x5A5A};
            /* cycles: the index, so that a record says where it belongs */
            if (ring.write((u16)tp::Id::Test, i, p, sizeof(p)))
                i++;
        }
    });

    std::vector<TraceRing::Record> out(want);
    for (int round = 0; round < 200; round++)
    {
        u64 complete;
        ulong kept = tp::read_window(ring, 0, want, tp::ALL_EVENTS, 0, out.data(), complete);
        for (ulong i = 0; i < kept; i++)
        {
            const TraceRing::Record& x = out[i];
            INVARIANT(x.cycles == x.index && x.payload[0] == x.index && x.payload[1] == (x.index ^ 0x5A5A),
                      "record %lu read torn", (ulong)x.index);
            if (i != 0)
                INVARIANT(x.index == out[i - 1].index + 1, "the part skips from %lu to %lu",
                          (ulong)out[i - 1].index, (ulong)x.index);
        }
        if (kept != 0)
        {
            /* Whole from its oldest on, or from the ring's start */
            INVARIANT(complete == 0 ? out[0].index == 0 : complete <= out[0].cycles,
                      "a part from %lu says it is whole from %lu", (ulong)out[0].index, (ulong)complete);
            if (complete != 0 && kept < want)
                fuzz::reached("a part ended by a record written over as it was read");
        }
    }
    stop.store(true);
    writer.join();
    ring.release();
    INVARIANT(fuzz::heap_blocks() == blocks, "the ring kept %zu blocks", fuzz::heap_blocks() - blocks);
}

void reset()
{
    fuzz::heap_fail_after(-1);
}

void run(fuzz::Input& in)
{
    switch (in.u8() % 16)
    {
    case 0:
        run_sort(in);
        break;
    case 1:
        run_top(in);
        break;
    case 2:
        run_duration(in);
        break;
    case 3:
        run_threads(in);
        break;
    default:
        run_window(in);
        break;
    }
}

}

const fuzz::Target fuzz::THE_TARGET = {"tracewin", reset, run, 512, 20000};
