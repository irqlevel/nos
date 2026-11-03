#include "tracepoint.h"
#include "trace_ring.h"

#include "cpu.h"
#include "klog.h"
#include "mutex.h"
#include "panic.h"
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

const Event EVENTS[COUNT] = {
    { "test", "test", "what `trace test` records: a sequence number and a value",
      sizeof(Test), sizeof(TEST_FIELDS) / sizeof(TEST_FIELDS[0]), TEST_FIELDS },
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

/* The newest `count` records of every ring (or of cpu `only`), merged by
   their cycle counts, of the events `filter` names. Under control(). */
void show(stdlib::Printer& con, ulong count, ulong only, const char* const* filter, ulong filter_count)
{
    Window windows[MAX_CPUS] = {};
    ulong total = 0;

    /* The clock and the counter together, once: every record's time is
       counted back from them. */
    ulong flags = hal::irq_save();
    u64 now_cycles = hal::read_cycle_counter();
    ulong now_ns = get_boot_time().get_value();
    hal::irq_restore(flags);

    for (ulong cpu = 0; cpu < MAX_CPUS; cpu++)
    {
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

    /* Merged oldest first, the oldest past the newest `count` passed over. */
    ulong skip = (total > count) ? total - count : 0;
    for (ulong taken = 0; taken < total; taken++)
    {
        ulong best = MAX_CPUS;
        for (ulong cpu = 0; cpu < MAX_CPUS; cpu++)
        {
            Window& w = windows[cpu];
            if (w.next >= w.count)
                continue;
            if (best == MAX_CPUS || w.records[w.next].cycles < windows[best].records[windows[best].next].cycles)
                best = cpu;
        }
        if (best == MAX_CPUS)
            break;

        Window& w = windows[best];
        const TraceRing::Record& r = w.records[w.next++];
        if (taken < skip)
            continue;
        print_record(con, r, record_ns(r.cycles, now_cycles, now_ns));
    }

    for (ulong cpu = 0; cpu < MAX_CPUS; cpu++)
    {
        if (windows[cpu].records != nullptr)
            mm::free(windows[cpu].records);
    }
    if (total == 0)
        con.printf("trace: nothing recorded%s\n", have_rings() ? "" : " -- no event has been turned on");
}

void stat(stdlib::Printer& con)
{
    const ulong kib = ring_size_kib();
    con.printf("trace: rings of %lu KiB a cpu (%lu records)%s%s\n", kib, kib * consts::KB / TraceRing::SLOT_BYTES,
        have_rings() ? "" : ", none made yet",
        __atomic_load_n(&frozen, __ATOMIC_RELAXED) ? ", frozen" : "");
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
    else if (stdlib::str_cmp(verb, "show") == 0)
    {
        ulong count = SHOW_DEFAULT;
        ulong only = MAX_CPUS;
        const char* filter[WORDS_MAX];
        ulong filter_count = 0;
        for (ulong w = 1; w < n; w++)
        {
            ulong value;
            if (stdlib::mem_cmp(words[w], "cpu=", 4) == 0 && stdlib::parse_ulong(words[w] + 4, value) && value < MAX_CPUS)
                only = value;
            else if (stdlib::parse_ulong(words[w], value) && value != 0)
                count = (value < SHOW_MAX) ? value : SHOW_MAX;
            else
                filter[filter_count++] = words[w];
        }
        stdlib::AutoLock lock(control());
        show(con, count, only, filter, filter_count);
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
        con.printf("usage: trace [list] | on <event|group|all>... | off <...> | show [n] [cpu=N] [event...] | stat | "
            "clear | size <KiB> | free | freeze | thaw | test [n] | bench\n");
    }
}

}
}
