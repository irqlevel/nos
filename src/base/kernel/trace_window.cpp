#include "trace_window.h"

namespace kernel
{
namespace tp
{

bool keeps(const Select& select, const TraceRing::Record& r)
{
    if (select.mask != ALL_EVENTS && (event_bit(r.id) & select.mask) == 0)
        return false;
    for (ulong i = 0; i < select.filter_count && i < FIELD_FILTERS_MAX; i++)
    {
        const FieldFilter& f = select.filters[i];
        /* An event this kernel does not know has no field of any name */
        if (r.id >= COUNT)
            return false;
        ulong offset = f.offset[r.id];
        ulong size = f.size[r.id];
        if (size == 0 || size > sizeof(u64) || offset + size > sizeof(r.payload) || offset + size > r.size)
            return false;
        u64 v = 0;
        stdlib::mem_cpy(&v, reinterpret_cast<const u8*>(r.payload) + offset, size);
        if (v != f.value[r.id])
            return false;
    }
    return true;
}

ulong read_window(const TraceRing& ring, u64 from, ulong want, const Select& select, u32 cpu,
    TraceRing::Record* out, u64& complete_from)
{
    complete_from = 0;
    const u64 head = ring.head();
    const u64 cap = ring.capacity();
    if (want == 0 || out == nullptr || cap == 0 || head <= from)
        return 0;

    /* What the ring still holds of what was written since `from`: an older
       part written over is lost to the window */
    u64 oldest = from;
    bool cut = false;
    if (head - from > cap)
    {
        oldest = head - cap;
        cut = true;
    }

    const bool everything = (select.mask == ALL_EVENTS && select.filter_count == 0);
    ulong kept = 0;
    u64 oldest_read = COMPLETE_NEVER;
    for (u64 index = head; index > oldest; index--)
    {
        /* Read into the next free place: kept there only if it is one of
           the window's */
        TraceRing::Record& r = out[kept];
        if (!ring.read(index - 1, r))
        {
            /* Written over as it was read, or being written now: every
               older record is gone too, the ring being written in order */
            cut = true;
            break;
        }
        oldest_read = r.cycles;
        if (!everything && !keeps(select, r))
            continue;
        r.cpu = cpu;
        kept++;
        if (kept == want)
        {
            /* Room for no more: what is older than this was not looked at */
            if (index - 1 > oldest)
                cut = true;
            break;
        }
    }

    if (cut)
        complete_from = oldest_read;

    for (ulong i = 0; i < kept / 2; i++)
    {
        TraceRing::Record t = out[i];
        out[i] = out[kept - 1 - i];
        out[kept - 1 - i] = t;
    }
    return kept;
}

bool merge_next(Window* windows, ulong count, const TraceRing::Record*& out)
{
    ulong best = count;
    for (ulong i = 0; i < count; i++)
    {
        const Window& w = windows[i];
        if (w.next >= w.count)
            continue;
        if (best == count || w.records[w.next].cycles < windows[best].records[windows[best].next].cycles)
            best = i;
    }
    if (best == count)
        return false;
    out = &windows[best].records[windows[best].next++];
    return true;
}

u64 window_start(const Window* windows, ulong count, ulong& cpu)
{
    u64 start = 0;
    cpu = count;
    for (ulong i = 0; i < count; i++)
    {
        if (windows[i].complete_from > start)
        {
            start = windows[i].complete_from;
            cpu = i;
        }
    }
    return start;
}

WakePairing::WakePairing(WakeWait* waits, ulong room, u64 start, ulong (*to_ns)(u64 delta))
    : waits_(waits)
    , room_((waits != nullptr) ? room : 0)
    , start_(start)
    , to_ns_(to_ns)
    , ran_(0)
    , wakeups_(0)
    , superseded_(0)
    , strange_(0)
    , before_start_(0)
    , dropped_(0)
{
    stdlib::mem_set(wake_at_, 0, sizeof(wake_at_));
    stdlib::mem_set(late_ns_, 0, sizeof(late_ns_));
    stdlib::mem_set(wake_cpu_, 0, sizeof(wake_cpu_));
    stdlib::mem_set(pending_, 0, sizeof(pending_));
}

namespace
{

const u64 U32_MOST = 0xFFFFFFFFULL;
const u32 U16_MOST = 0xFFFF;

u16 cpu_of(u32 cpu)
{
    return (u16)((cpu < U16_MOST) ? cpu : U16_MOST);
}

}

void WakePairing::feed(const TraceRing::Record& r)
{
    if (r.cycles < start_)
    {
        before_start_++;
        return;
    }

    /* Payloads through mem_cpy: a record's words are no struct of either
       event's, and its size is checked against the event's before */
    if (r.id == (u16)Id::SchedWakeup && r.size == sizeof(SchedWakeup))
    {
        SchedWakeup w;
        stdlib::mem_cpy(&w, r.payload, sizeof(w));
        wakeups_++;
        if (w.pid >= PID_SLOTS)
        {
            strange_++;
            return;
        }
        /* Woken again with no switch to it between: the switch after the
           first is what the window lost, and this one is what the next
           switch answers */
        if (pending_[w.pid] != 0)
            superseded_++;
        wake_at_[w.pid] = r.cycles;
        late_ns_[w.pid] = w.late_ns;
        wake_cpu_[w.pid] = cpu_of(w.cpu);
        pending_[w.pid] = 1;
        return;
    }

    if (r.id == (u16)Id::SchedSwitch && r.size == sizeof(SchedSwitch))
    {
        SchedSwitch s;
        stdlib::mem_cpy(&s, r.payload, sizeof(s));
        if (s.next_pid >= PID_SLOTS || pending_[s.next_pid] == 0)
            return;
        pending_[s.next_pid] = 0;
        if (ran_ >= room_)
        {
            dropped_++;
            return;
        }

        /* The merge feeds a switch after the wakeup it answers: never an
           older cycle count, but a stale one is a wait of 0 rather than a
           wrap */
        u64 at = wake_at_[s.next_pid];
        u64 ns = (r.cycles > at) ? to_ns_(r.cycles - at) : 0;
        u64 late = late_ns_[s.next_pid];
        ns = (ns > ~(u64)0 - late) ? ~(u64)0 : ns + late;

        WakeWait& ww = waits_[ran_++];
        ww.ns = (u32)((ns > U32_MOST) ? U32_MOST : ns);
        ww.pid = s.next_pid;
        ww.woken_cpu = wake_cpu_[s.next_pid];
        ww.ran_cpu = cpu_of(r.cpu);
        stdlib::mem_set(ww.comm, 0, sizeof(ww.comm));
        stdlib::mem_cpy(ww.comm, s.next_comm, COMM_BYTES);
    }
}

ulong WakePairing::pending() const
{
    ulong n = 0;
    for (ulong pid = 0; pid < PID_SLOTS; pid++)
        n += (pending_[pid] != 0) ? 1 : 0;
    return n;
}

namespace
{

/* The heap below `n` with its root at `root` put right: the larger child
   swapped up until neither is larger. A child's index past `n` -- or past
   what an index holds -- is none. */
void sift_down(u32* v, ulong root, ulong n)
{
    for (;;)
    {
        ulong child = 2 * root + 1;
        if (child >= n || child < root)
            return;
        if (child + 1 < n && v[child + 1] > v[child])
            child++;
        if (v[root] >= v[child])
            return;
        u32 t = v[root];
        v[root] = v[child];
        v[child] = t;
        root = child;
    }
}

}

void sort_u32(u32* v, ulong n)
{
    if (n < 2)
        return;
    for (ulong i = n / 2; i > 0; i--)
        sift_down(v, i - 1, n);
    for (ulong end = n - 1; end > 0; end--)
    {
        u32 t = v[0];
        v[0] = v[end];
        v[end] = t;
        sift_down(v, 0, end);
    }
}

ulong top_waits(const WakeWait* waits, ulong n, ulong* out, ulong k)
{
    ulong kept = 0;
    for (ulong i = 0; i < n; i++)
    {
        /* Its place: after every one kept that is as long or longer */
        ulong at = kept;
        while (at > 0 && waits[out[at - 1]].ns < waits[i].ns)
            at--;
        if (at >= k)
            continue;
        ulong move = (kept < k) ? kept : k - 1;
        while (move > at)
        {
            out[move] = out[move - 1];
            move--;
        }
        out[at] = i;
        if (kept < k)
            kept++;
    }
    return kept;
}

ulong percentile_index(ulong n, ulong p)
{
    const ulong HUNDRED = 100;
    if (n == 0)
        return 0;
    if (p > HUNDRED)
        p = HUNDRED;
    return (n - 1) * p / HUNDRED;
}

bool parse_duration(const char* text, u64& ns)
{
    /* A billion of any unit is more than any wait this measures, and keeps
       the product below from wrapping: 10^9 s is 10^18 ns */
    const u64 MOST = 1000000000ULL;
    const u64 TEN = 10;

    if (text == nullptr)
        return false;
    const char* p = text;
    u64 value = 0;
    ulong digits = 0;
    while (*p >= '0' && *p <= '9')
    {
        value = value * TEN + (u64)(*p - '0');
        if (value > MOST)
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

}
}
