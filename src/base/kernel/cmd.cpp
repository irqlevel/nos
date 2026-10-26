#include "cmd.h"
#include "version_gen.h"
#include "trace.h"
#include <hal/cpu.h>
#include <hal/console.h>
#include "dmesg.h"
#include "cpu.h"
#include "interrupt.h"
#include "time.h"
#include "watchdog.h"
#include "parameters.h"
#include "console.h"
#include "mutex.h"
#include "event.h"
#include "task.h"
#include "stack_probe.h"
#include "symtab.h"
#include "profiler.h"
#include "module.h"

#include <drivers/vga.h>
#include <drivers/pci.h>
#ifdef __x86_64__
#endif
#include <include/const.h>
#include <mm/page_table.h>
#include <mm/memory_map.h>
#include <mm/new.h>
#include <lib/unique_ptr.h>
#include <lib/checksum.h>
#include <lib/grub_env.h>

/* The network layer is Rust (src/crates/net); its commands register
   themselves. These two are boot's, run by the shell's own task. */
extern "C" {
void rust_net_dhcp_auto(void* printer);
void rust_net_dhcp_stop();
}

/* The filesystem layer is Rust (src/crates/fs). These are the calls the shell
   makes on it -- a script to read and rewrite, a file to checksum, a download
   to write -- taking paths as bytes and a length rather than a C string.

   kernel_file_* are the whole-file calls, and they know about the pair a
   write leaves behind: a content written through kernel_file_write goes to
   <path>.new first and takes the old file's place only once it is whole on
   disk, and the reads below look in both places. */
extern "C" {

struct RustFile;

/* Open flags (crate::vfs) */
static const ulong FILE_READ = 1;
static const ulong FILE_WRITE = 2;

RustFile* kernel_vfs_open(const char* path, ulong len, ulong flags);
void kernel_vfs_close(RustFile* file);
int kernel_vfs_read(RustFile* file, void* buf, ulong len, ulong* out);
int kernel_vfs_write(RustFile* file, const void* data, ulong len);
ulong kernel_vfs_size(RustFile* file);
int kernel_vfs_remove(const char* path, ulong len);
int kernel_vfs_sync();

long kernel_file_size(const char* path, ulong len);
long kernel_file_read(const char* path, ulong len, void* buf, ulong cap);
int kernel_file_write(const char* path, ulong len, const void* data, ulong data_len);
int kernel_file_remove(const char* path, ulong len);
int kernel_dir_create(const char* path, ulong len);

}

namespace kernel
{

/* What a path fits in, NUL included (crate::vfs::MAX_PATH) */
static const ulong MAX_PATH = 256;

/* The shell holds paths as C strings; the layer takes bytes and a length. */
static RustFile* file_open(const char* path, ulong flags)
{
    return kernel_vfs_open(path, stdlib::str_len(path), flags);
}


struct CmdEntry
{
    const char* name;
    void (*handler)(const char* args, stdlib::Printer& con);
    const char* help;
};

static void cmd_cls(const char* args, stdlib::Printer& con)
{
    (void)args;
    Console::get_instance().cls();
    (void)con;
}

static void cmd_poweroff(const char* args, stdlib::Printer& con)
{
    (void)args;
    con.printf("shutting down...\n");
    Cmd::get_instance().request_shutdown();
}

static void cmd_reboot(const char* args, stdlib::Printer& con)
{
    (void)args;
    con.printf("rebooting...\n");
    Cmd::get_instance().request_reboot();
}

static void cmd_lscpu(const char* args, stdlib::Printer& con)
{
    (void)args;

    auto& cpus = CpuTable::get_instance();
    ulong mask = cpus.get_running_cpus();

    ulong count = 0;
    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        if (mask & (1UL << i))
            count++;
    }

    con.printf("cpus %lu running, id mask 0x%lX, bsp %lu\n",
        count, mask, cpus.get_bsp_index_no_lock());

    hal::print_cpu_info(con);
}

static void cmd_cpu(const char* args, stdlib::Printer& con)
{
    (void)args;
    hal::print_cpu_state(con);
}

static void cmd_dmesg(const char* args, stdlib::Printer& con)
{
    const char* end;
    const char* tok = stdlib::next_token(args, end);

    char filter[64];
    filter[0] = '\0';
    if (tok)
        stdlib::token_copy(tok, end, filter, sizeof(filter));

    /* A numeric first argument is a line count, not a filter: "dmesg 40" is
       the newest 40 lines, "dmesg nvme" filters the whole log, "dmesg 40 nvme"
       does both. The count is what makes the command usable over the remote
       shell, whose reply buffer holds about forty lines and drops the rest --
       so without it every dmesg returns the first minute of boot. */
    ulong last_lines = 0;
    ulong count;
    if (filter[0] != '\0' && stdlib::parse_ulong(filter, count))
    {
        last_lines = count;
        filter[0] = '\0';

        tok = stdlib::next_token(end, end);
        if (tok)
            stdlib::token_copy(tok, end, filter, sizeof(filter));
    }

    Dmesg::get_instance().dump(con, last_lines,
                              (filter[0] != '\0') ? filter : nullptr);
}

static void cmd_loglevel(const char* args, stdlib::Printer& con)
{
    auto& tracer = Tracer::get_instance();

    const char* end;
    const char* tok = stdlib::next_token(args, end);
    if (!tok)
    {
        con.printf("loglevel %lu\n", (ulong)tracer.get_level());
        return;
    }

    char buf[16];
    stdlib::token_copy(tok, end, buf, sizeof(buf));

    ulong level;
    if (!stdlib::parse_ulong(buf, level) || level > (ulong)MAX_TRACE_LEVEL)
    {
        con.printf("usage: loglevel [0-%lu]\n", (ulong)MAX_TRACE_LEVEL);
        return;
    }

    /* The per-subsystem levels in trace.h are compile-time constants, so the
       only way to see what xHCI or the page allocator is doing used to be a
       rebuild and a reflash -- on a machine whose console is a UDP socket that
       is most of an afternoon. Raising the level is loud: level 4 traces every
       allocation, and every line also goes out the serial port. */
    tracer.set_level((int)level);
    con.printf("loglevel %lu\n", level);
}

static void cmd_uptime(const char* args, stdlib::Printer& con)
{
    (void)args;
    auto time = get_boot_time();
    con.printf("%lu.%lu\n", time.get_secs(), time.get_usecs());
}

static void cmd_date(const char* args, stdlib::Printer& con)
{
    (void)args;

    static const ulong SECS_PER_MIN  = 60;
    static const ulong MINS_PER_HOUR = 60;
    static const ulong HOURS_PER_DAY = 24;
    static const ulong DAYS_PER_YEAR = 365;
    static const ulong DAYS_PER_LEAP_YEAR = 366;
    static const ulong MONTHS_PER_YEAR = 12;
    static const ulong FEBRUARY_INDEX = 2;
    static const ulong UNIX_EPOCH_YEAR = 1970;

    ulong epoch = get_wall_time_secs();
    if (epoch == 0)
    {
        con.printf("wall clock not available\n");
        return;
    }

    /* Decompose Unix epoch into Y/M/D H:M:S */
    ulong secs = epoch;
    ulong s = secs % SECS_PER_MIN; secs /= SECS_PER_MIN;
    ulong m = secs % MINS_PER_HOUR; secs /= MINS_PER_HOUR;
    ulong h = secs % HOURS_PER_DAY; secs /= HOURS_PER_DAY;

    ulong days = secs; /* days since 1970-01-01 */
    ulong y = UNIX_EPOCH_YEAR;
    while (true)
    {
        bool leap = (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0);
        ulong days_in_year = leap ? DAYS_PER_LEAP_YEAR : DAYS_PER_YEAR;
        if (days < days_in_year)
            break;
        days -= days_in_year;
        y++;
    }

    static const u16 DAYS_IN_MONTH[13] = {
        0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31
    };
    bool leap = (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0);
    ulong mo = 1;
    while (mo <= MONTHS_PER_YEAR)
    {
        ulong dim = DAYS_IN_MONTH[mo];
        if (mo == FEBRUARY_INDEX && leap)
            dim++;
        if (days < dim)
            break;
        days -= dim;
        mo++;
    }
    ulong d = days + 1;

    con.printf("%lu-%lu-%lu %lu:%lu:%lu UTC\n", y, mo, d, h, m, s);
}

/* Percentages are per CPU, the way top has always reported them: a thread
   pinning one core reads 100%, and on a 20-CPU box the column can total
   2000%. Two samples a moment apart, because a cumulative runtime tells you
   what a task has ever done, not what it is doing. */
static void cmd_top(const char* args, stdlib::Printer& con)
{
    static const ulong MAX_SAMPLES = 256;
    static const ulong DEFAULT_INTERVAL_MS = 500;
    static const ulong MAX_INTERVAL_MS = 10000;
    static const ulong TAG = 'Top ';

    const char* end;
    const char* tok = stdlib::next_token(args, end);

    ulong interval_ms = DEFAULT_INTERVAL_MS;
    if (tok)
    {
        char buf[16];
        stdlib::token_copy(tok, end, buf, sizeof(buf));

        ulong ms;
        if (!stdlib::parse_ulong(buf, ms) || ms == 0 || ms > MAX_INTERVAL_MS)
        {
            con.printf("usage: top [interval-ms, 1-%lu]\n", MAX_INTERVAL_MS);
            return;
        }
        interval_ms = ms;
    }

    ulong bytes = MAX_SAMPLES * sizeof(TaskTable::CpuSample);
    auto* before = (TaskTable::CpuSample*)mm::alloc(bytes, TAG);
    if (!before)
    {
        con.printf("top: out of memory\n");
        return;
    }

    auto* after = (TaskTable::CpuSample*)mm::alloc(bytes, TAG);
    if (!after)
    {
        mm::free(before);
        con.printf("top: out of memory\n");
        return;
    }

    auto& table = TaskTable::get_instance();

    /* Clock read before the first sample and after the second, so the window
       is never narrower than the runtime it is dividing: a task then reads a
       shade under its true share rather than a shade over 100%. */
    ulong t0 = get_boot_time().get_value();
    size_t n0 = table.sample_cpu(before, MAX_SAMPLES);
    long deferred0 = get_preempt_deferred_count();
    long deferred_idle0 = get_preempt_deferred_idle_count();

    sleep(interval_ms * consts::NANO_SECS_IN_MS);

    size_t n1 = table.sample_cpu(after, MAX_SAMPLES);
    long deferred1 = get_preempt_deferred_count();
    long deferred_idle1 = get_preempt_deferred_idle_count();
    ulong elapsed = get_boot_time().get_value() - t0;

    if (elapsed == 0)
    {
        mm::free(after);
        mm::free(before);
        con.printf("top: no time passed\n");
        return;
    }

    /* Tenths of a percent, so the column keeps one decimal without floats. */
    ulong permille[MAX_SAMPLES];
    for (size_t i = 0; i < n1; i++)
    {
        ulong was = 0;
        for (size_t j = 0; j < n0; j++)
        {
            if (before[j].pid == after[i].pid)
            {
                was = before[j].runtime_ns;
                break;
            }
        }

        ulong delta = (after[i].runtime_ns > was) ? (after[i].runtime_ns - was) : 0;
        permille[i] = (delta * 1000) / elapsed;
    }

    /* Selection sort: a couple of hundred entries at most, and it keeps the
       shell free of anything that allocates. */
    for (size_t i = 0; i < n1; i++)
    {
        size_t best = i;
        for (size_t j = i + 1; j < n1; j++)
        {
            if (permille[j] > permille[best])
                best = j;
        }

        if (best != i)
        {
            ulong p = permille[i]; permille[i] = permille[best]; permille[best] = p;
            TaskTable::CpuSample t = after[i]; after[i] = after[best]; after[best] = t;
        }
    }

    ulong cpus = 0;
    ulong mask = CpuTable::get_instance().get_running_cpus();
    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        if (mask & (1UL << i))
            cpus++;
    }

    ulong busy = 0;
    for (size_t i = 0; i < n1; i++)
    {
        if (stdlib::str_str(after[i].name, "idle") != after[i].name)
            busy += permille[i];
    }

    con.printf("cpus %lu, %lu tasks, %lu ms window, busy %lu.%lu%% of %lu00%%\n",
        cpus, (ulong)n1, interval_ms, busy / 10, busy % 10, cpus);
    con.printf("  pid    cpu%%  name\n");

    for (size_t i = 0; i < n1; i++)
    {
        con.printf("%lu %lu.%lu %s\n", after[i].pid,
            permille[i] / 10, permille[i] % 10, after[i].name);
    }

    mm::free(after);
    mm::free(before);

    con.printf("task migrations since boot: %lu\n", (ulong)get_task_migration_count());
    /* The window's count first: it is what the load being looked at did. The
       one since boot starts at one per CPU, on its idle task, that the boot
       self-test makes on purpose (test_preempt_deferred). */
    con.printf("preemptions deferred: %lu in the window, %lu of them on an idle task; "
        "%lu since boot, %lu on an idle task\n",
        (ulong)(deferred1 - deferred0), (ulong)(deferred_idle1 - deferred_idle0),
        (ulong)deferred1, (ulong)deferred_idle1);
}

/* A wait as the SchedStats bucket it fell in, an upper bound: "<4us" */
static void sched_bucket_text(ulong bucket, char* buf, size_t size)
{
    if (bucket == ~0UL)
    {
        stdlib::sn_printf(buf, size, "-");
        return;
    }

    static const ulong US_IN_MS = 1000;
    static const ulong US_IN_SEC = 1000 * US_IN_MS;

    /* The last bucket has no upper bound: everything from its floor up */
    bool last = (bucket + 1 >= SchedStats::BUCKETS);
    ulong us = last ? SchedStats::bucket_limit_us(bucket - 1) : SchedStats::bucket_limit_us(bucket);
    const char* rel = last ? ">=" : "<";

    if (us < US_IN_MS)
        stdlib::sn_printf(buf, size, "%s%luus", rel, us);
    else if (us < US_IN_SEC)
        stdlib::sn_printf(buf, size, "%s%lu.%lums", rel, us / US_IN_MS, (us % US_IN_MS) / (US_IN_MS / 10));
    else
        stdlib::sn_printf(buf, size, "%s%lu.%lus", rel, us / US_IN_SEC, (us % US_IN_SEC) / (US_IN_SEC / 10));
}

/* Nanoseconds, short: "410ns", "6.2us", "1.3ms", "2.1s" */
static void sched_ns_text(ulong ns, char* buf, size_t size)
{
    if (ns < consts::NANO_SECS_IN_USEC)
        stdlib::sn_printf(buf, size, "%luns", ns);
    else if (ns < consts::NANO_SECS_IN_MS)
        stdlib::sn_printf(buf, size, "%lu.%luus", ns / consts::NANO_SECS_IN_USEC,
            (ns % consts::NANO_SECS_IN_USEC) / 100);
    else if (ns < consts::NANO_SECS_IN_SEC)
        stdlib::sn_printf(buf, size, "%lu.%lums", ns / consts::NANO_SECS_IN_MS,
            (ns % consts::NANO_SECS_IN_MS) / (100 * consts::NANO_SECS_IN_USEC));
    else
        stdlib::sn_printf(buf, size, "%lu.%lus", ns / consts::NANO_SECS_IN_SEC,
            (ns % consts::NANO_SECS_IN_SEC) / (100 * consts::NANO_SECS_IN_MS));
}

/* The bucket that the given per mille of a histogram's entries are at or
   under; ~0UL for an empty one */
static ulong sched_percentile(const ulong* hist, ulong permille)
{
    ulong count = 0;
    for (ulong i = 0; i < SchedStats::BUCKETS; i++)
        count += hist[i];
    if (count == 0)
        return ~0UL;

    ulong want = (count * permille + 999) / 1000;
    ulong seen = 0;
    for (ulong i = 0; i < SchedStats::BUCKETS; i++)
    {
        seen += hist[i];
        if (seen >= want)
            return i;
    }
    return SchedStats::BUCKETS - 1;
}

/* One line of `sched`: what a window's difference of two snapshots says,
   and the since-boot maxima of the later one */
static void sched_line(const char* who, ulong tasks, const SchedStats::Snapshot& d,
    const SchedStats::Snapshot& now, stdlib::Printer& con)
{
    ulong wakes = 0, delays = 0;
    for (ulong i = 0; i < SchedStats::BUCKETS; i++)
    {
        wakes += d.wake[i];
        delays += d.delay[i];
    }

    char w50[16], w99[16], w999[16], wmax[16], d50[16], d99[16], cavg[16], cmax[16];
    sched_bucket_text(sched_percentile(d.wake, 500), w50, sizeof(w50));
    sched_bucket_text(sched_percentile(d.wake, 990), w99, sizeof(w99));
    sched_bucket_text(sched_percentile(d.wake, 999), w999, sizeof(w999));
    sched_ns_text(now.wake_max_ns, wmax, sizeof(wmax));
    sched_bucket_text(sched_percentile(d.delay, 500), d50, sizeof(d50));
    sched_bucket_text(sched_percentile(d.delay, 990), d99, sizeof(d99));

    ulong decisions = d.decisions;
    sched_ns_text((decisions != 0) ? cycle_counter_delta_to_ns(d.decision_cycles) / decisions : 0,
        cavg, sizeof(cavg));
    sched_ns_text(cycle_counter_delta_to_ns(now.decision_max_cycles), cmax, sizeof(cmax));

    /* Tenths, so averages keep a decimal without floats */
    ulong touch10 = (decisions != 0) ? (d.touched * 10) / decisions : 0;
    ulong run10 = (d.looks != 0) ? (d.runnable * 10) / d.looks : 0;
    ulong crowded = (d.looks != 0) ? (d.crowded * 100) / d.looks : 0;

    con.printf("%-4s %5lu %7lu %7s %7s %7s %7s %6lu %7s %7s %7lu %7s %7s %3lu.%lu %4lu %3lu.%lu %3lu %3lu%%\n",
        who, tasks, wakes, w50, w99, w999, wmax, delays, d50, d99,
        decisions, cavg, cmax, touch10 / 10, touch10 % 10, now.touched_max,
        run10 / 10, run10 % 10, now.runnable_max, crowded);
}

static void sched_sum(SchedStats::Snapshot& into, const SchedStats::Snapshot& s)
{
    for (ulong i = 0; i < SchedStats::BUCKETS; i++)
    {
        into.wake[i] += s.wake[i];
        into.delay[i] += s.delay[i];
    }
    into.wake_max_ns = stdlib::max(into.wake_max_ns, s.wake_max_ns);
    into.decisions += s.decisions;
    into.decision_cycles += s.decision_cycles;
    into.decision_max_cycles = stdlib::max(into.decision_max_cycles, s.decision_max_cycles);
    into.touched += s.touched;
    into.touched_max = stdlib::max(into.touched_max, s.touched_max);
    into.looks += s.looks;
    into.runnable += s.runnable;
    into.runnable_max = stdlib::max(into.runnable_max, s.runnable_max);
    into.crowded += s.crowded;
}

/* d = b - a, the counters' part; the maxima are b's, since boot */
static void sched_diff(SchedStats::Snapshot& d, const SchedStats::Snapshot& a,
    const SchedStats::Snapshot& b)
{
    for (ulong i = 0; i < SchedStats::BUCKETS; i++)
    {
        d.wake[i] = b.wake[i] - a.wake[i];
        d.delay[i] = b.delay[i] - a.delay[i];
    }
    d.wake_max_ns = b.wake_max_ns;
    d.decisions = b.decisions - a.decisions;
    d.decision_cycles = b.decision_cycles - a.decision_cycles;
    d.decision_max_cycles = b.decision_max_cycles;
    d.touched = b.touched - a.touched;
    d.touched_max = b.touched_max;
    d.looks = b.looks - a.looks;
    d.runnable = b.runnable - a.runnable;
    d.runnable_max = b.runnable_max;
    d.crowded = b.crowded - a.crowded;
}

/* `sched probe`: how long a woken task waits for a CPU that others keep
   busy -- the case a scheduler is chosen by. Spinners pinned to one CPU that
   never yield, so only the tick takes it from them; a waiter pinned beside
   them, asleep on an Event; and this command's task, on another CPU,
   signalling it a sample at a time and waiting to hear that it ran. Each
   wait is from the signal() to the waiter running: the waker's IPI, and
   whatever the scheduler makes it wait behind. */
struct SchedProbe
{
    Event ev;
    Atomic stop;
    /* When the signal being answered was sent */
    Atomic sent_at;
    Atomic answered;
    /* The waiter's own, read once it has exited: its exit and the join
       order them */
    ulong hist[SchedStats::BUCKETS];
    ulong max_ns;
    ulong count;
    /* The lags the waiter was placed with as it woke, virtual ns */
    s64 lag_min;
    s64 lag_max;
    s64 lag_sum;

    static const ulong TAG = 'SPrb';
};

static const ulong SCHED_PROBE_MAX_SPINNERS = 16;
static const ulong SCHED_PROBE_MAX_MS = 60000;
/* How long the waiter waits before it looks at Stop again */
static const ulong SCHED_PROBE_WAIT_NS = 100 * consts::NANO_SECS_IN_MS;
/* A signal not answered in this long is counted as such, and the probe
   moves on */
static const ulong SCHED_PROBE_ANSWER_NS = consts::NANO_SECS_IN_SEC;
/* Between one answer and the next signal: the waiter is asleep again by
   then */
static const ulong SCHED_PROBE_GAP_NS = 2 * consts::NANO_SECS_IN_MS;

static void sched_probe_spin(void* ctx)
{
    auto* p = static_cast<SchedProbe*>(ctx);
    while (p->stop.get() == 0)
        cpu_relax();
}

static void sched_probe_waiter(void* ctx)
{
    auto* p = static_cast<SchedProbe*>(ctx);
    while (p->stop.get() == 0)
    {
        if (!p->ev.wait_for(SCHED_PROBE_WAIT_NS))
            continue;

        ulong now = get_boot_time().get_value();
        ulong sent = (ulong)p->sent_at.get();
        if (sent != 0 && now >= sent)
        {
            ulong ns = now - sent;
            p->hist[SchedStats::bucket(ns)]++;
            p->max_ns = stdlib::max(p->max_ns, ns);
            p->count++;

            /* The lag it was placed with as it woke */
            s64 lag = Task::get_current_task()->sched_.placed_lag;
            p->lag_min = stdlib::min(p->lag_min, lag);
            p->lag_max = stdlib::max(p->lag_max, lag);
            p->lag_sum += lag;
        }
        p->sent_at.set(0);
        p->answered.set(1);
    }
}

/* Runtime of task so far, under its lock as every update_runtime() is made */
static ulong sched_probe_runtime(Task* task)
{
    stdlib::AutoLock lock(task->lock_);
    return task->runtime_.get_value();
}

static void cmd_sched_probe(const char* args, stdlib::Printer& con)
{
    auto& cpus = CpuTable::get_instance();
    ulong mask = cpus.get_running_cpus();
    ulong self = cpus.get_current_cpu_id();

    ulong spinners = 2;
    ulong ms = 2000;
    /* The highest CPU that is not this one, unless told: this task signals
       from here */
    ulong cpu = ~0UL;
    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        if ((mask & (1UL << i)) && i != self)
            cpu = i;
    }

    const char* end;
    for (const char* tok = stdlib::next_token(args, end); tok; tok = stdlib::next_token(end, end))
    {
        /* Zeroed: the prefixes are compared over their whole length, past
           the end of a token shorter than one */
        char buf[32] = {};
        stdlib::token_copy(tok, end, buf, sizeof(buf));

        ulong value;
        bool ok = false;
        if (stdlib::mem_cmp(buf, "spinners=", 9) == 0)
        {
            ok = stdlib::parse_ulong(buf + 9, value) && value <= SCHED_PROBE_MAX_SPINNERS;
            if (ok)
                spinners = value;
        }
        else if (stdlib::mem_cmp(buf, "ms=", 3) == 0)
        {
            ok = stdlib::parse_ulong(buf + 3, value) && value != 0 && value <= SCHED_PROBE_MAX_MS;
            if (ok)
                ms = value;
        }
        else if (stdlib::mem_cmp(buf, "cpu=", 4) == 0)
        {
            ok = stdlib::parse_ulong(buf + 4, value) && value < MAX_CPUS && (mask & (1UL << value));
            if (ok)
                cpu = value;
        }

        if (!ok)
        {
            con.printf("usage: sched probe [spinners=0-%lu] [ms=1-%lu] [cpu=N]\n",
                SCHED_PROBE_MAX_SPINNERS, SCHED_PROBE_MAX_MS);
            return;
        }
    }

    if (cpu == ~0UL)
    {
        con.printf("sched probe: needs a second cpu\n");
        return;
    }

    auto* p = mm::talloc<SchedProbe, SchedProbe::TAG>();
    if (p == nullptr)
    {
        con.printf("sched probe: out of memory\n");
        return;
    }
    stdlib::mem_set(p->hist, 0, sizeof(p->hist));
    p->max_ns = 0;
    p->lag_min = (s64)(~0UL >> 1);
    p->lag_max = -(s64)(~0UL >> 1);
    p->lag_sum = 0;
    p->count = 0;

    /* The waiter first, the spinners after it */
    Task* tasks[SCHED_PROBE_MAX_SPINNERS + 1] = {};
    ulong started = 0;
    bool ok = true;
    for (ulong i = 0; ok && i <= spinners; i++)
    {
        char name[TASK_NAME_LEN];
        if (i == 0)
            stdlib::sn_printf(name, sizeof(name), "probe/wait");
        else
            stdlib::sn_printf(name, sizeof(name), "probe/spin%lu", i - 1);

        Task* task = mm::talloc<Task, SchedProbe::TAG>(name);
        if (task == nullptr)
        {
            ok = false;
            break;
        }
        task->set_cpu_affinity(1UL << cpu);
        if (!task->start((i == 0) ? sched_probe_waiter : sched_probe_spin, p))
        {
            task->put();
            ok = false;
            break;
        }
        tasks[started++] = task;
    }

    ulong run_at[SCHED_PROBE_MAX_SPINNERS + 1] = {};
    ulong sent = 0, unanswered = 0;
    ulong t0 = get_boot_time().get_value();
    for (ulong i = 0; i < started; i++)
        run_at[i] = sched_probe_runtime(tasks[i]);

    ulong until = t0 + ms * consts::NANO_SECS_IN_MS;
    while (ok && get_boot_time().get_value() < until)
    {
        p->answered.set(0);
        p->sent_at.set((long)get_boot_time().get_value());
        p->ev.signal();
        sent++;

        ulong give_up = get_boot_time().get_value() + SCHED_PROBE_ANSWER_NS;
        while (p->answered.get() == 0 && get_boot_time().get_value() < give_up)
            sleep(consts::NANO_SECS_IN_MS);
        if (p->answered.get() == 0)
            unanswered++;

        sleep(SCHED_PROBE_GAP_NS);
    }
    ulong elapsed = get_boot_time().get_value() - t0;

    ulong ran[SCHED_PROBE_MAX_SPINNERS + 1] = {};
    for (ulong i = 0; i < started; i++)
        ran[i] = sched_probe_runtime(tasks[i]) - run_at[i];

    p->stop.set(1);
    p->ev.signal();
    for (ulong i = 0; i < started; i++)
    {
        tasks[i]->wait();
        tasks[i]->put();
    }

    if (!ok)
    {
        con.printf("sched probe: could not start its tasks\n");
    }
    else
    {
        char w50[16], w90[16], w99[16], wmax[16];
        sched_bucket_text(sched_percentile(p->hist, 500), w50, sizeof(w50));
        sched_bucket_text(sched_percentile(p->hist, 900), w90, sizeof(w90));
        sched_bucket_text(sched_percentile(p->hist, 990), w99, sizeof(w99));
        sched_ns_text(p->max_ns, wmax, sizeof(wmax));

        con.printf("sched probe: cpu %lu, %lu spinners, %lu ms: %lu signals, %lu answered, %lu not in %lu ms\n",
            cpu, spinners, ms, sent, p->count, unanswered, SCHED_PROBE_ANSWER_NS / consts::NANO_SECS_IN_MS);
        con.printf("  waited p50 %s p90 %s p99 %s max %s\n", w50, w90, w99, wmax);
        if (p->count != 0)
            con.printf("  woken with lag (virtual us): min %ld avg %ld max %ld\n",
                (long)(p->lag_min / (s64)consts::NANO_SECS_IN_USEC),
                (long)(p->lag_sum / (s64)p->count / (s64)consts::NANO_SECS_IN_USEC),
                (long)(p->lag_max / (s64)consts::NANO_SECS_IN_USEC));
        con.printf("  ran, per mille of the window:");
        for (ulong i = 0; i < started && elapsed != 0; i++)
            con.printf(" %s %lu", tasks[i]->get_name(), (ran[i] * 1000) / elapsed);
        con.printf("\n");
    }

    p->~SchedProbe();
    mm::free(p);
}

/* `sched queue [cpu]`: what EEVDF has of each task on a CPU's queue that
   can run, the one on the CPU first -- its nice and slice, its lag (V less
   its virtual runtime: what it is owed), how far past V its deadline is,
   and whether it is eligible. Copied under the queue's lock, printed after. */
static const ulong SCHED_QUEUE_MAX = 64;

static void cmd_sched_queue(const char* args, stdlib::Printer& con)
{
    auto& cpus = CpuTable::get_instance();
    ulong mask = cpus.get_running_cpus();
    ulong cpu = cpus.get_current_cpu_id();

    const char* end;
    const char* tok = stdlib::next_token(args, end);
    if (tok)
    {
        char buf[16];
        stdlib::token_copy(tok, end, buf, sizeof(buf));
        if (!stdlib::parse_ulong(buf, cpu) || cpu >= MAX_CPUS || !(mask & (1UL << cpu)))
        {
            con.printf("usage: sched queue [cpu]\n");
            return;
        }
    }

    auto* views = (TaskQueue::EntityView*)mm::alloc(SCHED_QUEUE_MAX * sizeof(TaskQueue::EntityView), SchedProbe::TAG);
    if (views == nullptr)
    {
        con.printf("sched: out of memory\n");
        return;
    }

    ulong n = cpus.get_cpu(cpu).get_task_queue().view(views, SCHED_QUEUE_MAX);
    con.printf("cpu %lu: %lu that can run, lag and deadline in virtual us from V\n", cpu, n);
    con.printf("  %-6s %-5s %6s %9s %10s %10s %s\n", "pid", "nice", "slice", "lag", "deadline", "eligible", "name");
    for (ulong i = 0; i < n; i++)
    {
        auto& e = views[i];
        con.printf("%s %-6lu %-5ld %6lu %9ld %10ld %10s %s\n", e.on_cpu ? "*" : " ", e.pid, e.nice,
            e.slice_ns / consts::NANO_SECS_IN_USEC, (long)(e.lag / (s64)consts::NANO_SECS_IN_USEC),
            (long)(e.deadline / (s64)consts::NANO_SECS_IN_USEC), e.eligible ? "yes" : "no", e.name);
    }

    mm::free(views);
}

/* `sched nice <pid> <nice>` and `sched slice <pid> <us>`: a task's share of
   its CPU, and how soon after it can it runs and for how long at a time */
static void cmd_sched_set(const char* args, stdlib::Printer& con, bool nice)
{
    const char* end;
    const char* tok = stdlib::next_token(args, end);
    char buf[24] = {};
    ulong pid = 0;
    if (tok)
        stdlib::token_copy(tok, end, buf, sizeof(buf));
    bool ok = tok && stdlib::parse_ulong(buf, pid);

    tok = ok ? stdlib::next_token(end, end) : nullptr;
    long value = 0;
    if (tok)
    {
        char vbuf[24] = {};
        stdlib::token_copy(tok, end, vbuf, sizeof(vbuf));
        bool negative = (vbuf[0] == '-');
        ulong magnitude = 0;
        ok = stdlib::parse_ulong(negative ? vbuf + 1 : vbuf, magnitude) && magnitude < (1UL << 32);
        value = negative ? -(long)magnitude : (long)magnitude;
    }
    else
    {
        ok = false;
    }

    const long slice_min_us = (long)(SchedEntity::MIN_SLICE_NS / consts::NANO_SECS_IN_USEC);
    const long slice_max_us = (long)(SchedEntity::MAX_SLICE_NS / consts::NANO_SECS_IN_USEC);
    if (!ok || (nice && (value < SchedEntity::NICE_MIN || value > SchedEntity::NICE_MAX)) ||
        (!nice && (value < slice_min_us || value > slice_max_us)))
    {
        con.printf("usage: sched nice <pid> <%ld..%ld> | sched slice <pid> <%ld..%ld us>\n",
            SchedEntity::NICE_MIN, SchedEntity::NICE_MAX, slice_min_us, slice_max_us);
        return;
    }

    ObjectPtr<Task> task(TaskTable::get_instance().lookup(pid));
    if (!task)
    {
        con.printf("task %lu not found\n", pid);
        return;
    }

    if (nice)
    {
        set_task_nice(task.get(), value);
        con.printf("%s (pid %lu): nice %ld\n", task->get_name(), pid, value);
    }
    else
    {
        set_task_slice(task.get(), (ulong)value * consts::NANO_SECS_IN_USEC);
        con.printf("%s (pid %lu): slice %ld us\n", task->get_name(), pid, value);
    }
}

/* What each CPU's scheduler did over a window (SchedStats): how long a woken
   task waited for the CPU -- at the median, the 99th and the 99.9th
   percentile, the buckets' bounds, and the longest since boot, exactly --
   how long one displaced while it could still run waited to run again, what
   a decision cost (the average, and the most since boot), how many tasks it
   touched, and how many tasks could run at the CPU's timer: on average, the
   most since boot, and how often two or more. */
static void cmd_sched(const char* args, stdlib::Printer& con)
{
    static const ulong DEFAULT_INTERVAL_MS = 1000;
    static const ulong MAX_INTERVAL_MS = 10000;
    static const ulong TAG = 'Schd';

    const char* end;
    const char* tok = stdlib::next_token(args, end);

    ulong interval_ms = DEFAULT_INTERVAL_MS;
    if (tok)
    {
        char buf[16];
        stdlib::token_copy(tok, end, buf, sizeof(buf));

        if (stdlib::str_cmp(buf, "probe") == 0)
        {
            cmd_sched_probe(end, con);
            return;
        }
        if (stdlib::str_cmp(buf, "queue") == 0)
        {
            cmd_sched_queue(end, con);
            return;
        }
        if (stdlib::str_cmp(buf, "nice") == 0 || stdlib::str_cmp(buf, "slice") == 0)
        {
            cmd_sched_set(end, con, buf[0] == 'n');
            return;
        }

        ulong ms;
        if (!stdlib::parse_ulong(buf, ms) || ms == 0 || ms > MAX_INTERVAL_MS)
        {
            con.printf("usage: sched [interval-ms, 1-%lu] | sched probe [spinners=N] [ms=M] [cpu=N] | "
                "sched queue [cpu] | sched nice <pid> <n> | sched slice <pid> <us>\n",
                MAX_INTERVAL_MS);
            return;
        }
        interval_ms = ms;
    }

    /* Three of them a CPU: before, after, and the difference */
    ulong bytes = 3 * MAX_CPUS * sizeof(SchedStats::Snapshot);
    auto* snaps = (SchedStats::Snapshot*)mm::alloc(bytes, TAG);
    if (!snaps)
    {
        con.printf("sched: out of memory\n");
        return;
    }
    SchedStats::Snapshot* before = &snaps[0];
    SchedStats::Snapshot* after = &snaps[MAX_CPUS];
    SchedStats::Snapshot* diff = &snaps[2 * MAX_CPUS];

    auto& cpus = CpuTable::get_instance();
    ulong mask = cpus.get_running_cpus();

    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        if (mask & (1UL << i))
            cpus.get_cpu(i).get_task_queue().read_stats(before[i]);
    }

    sleep(interval_ms * consts::NANO_SECS_IN_MS);

    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        if (mask & (1UL << i))
            cpus.get_cpu(i).get_task_queue().read_stats(after[i]);
    }

    con.printf("sched: %lu ms window; waits are bucket bounds, max since boot\n", interval_ms);
    con.printf("%-4s %5s %7s %7s %7s %7s %7s %6s %7s %7s %7s %7s %7s %5s %4s %5s %3s %4s\n",
        "cpu", "tasks", "wakeups", "p50", "p99", "p99.9", "max", "delays", "p50", "p99",
        "decide", "avg", "max", "touch", "max", "run", "max", "2+");

    SchedStats::Snapshot total;
    stdlib::mem_set(&total, 0, sizeof(total));
    SchedStats::Snapshot total_now;
    stdlib::mem_set(&total_now, 0, sizeof(total_now));
    ulong total_tasks = 0;

    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        if (!(mask & (1UL << i)))
            continue;

        sched_diff(diff[i], before[i], after[i]);
        ulong tasks = (ulong)cpus.get_cpu(i).get_task_queue().get_task_count();
        char who[8];
        stdlib::sn_printf(who, sizeof(who), "%lu", i);
        sched_line(who, tasks, diff[i], after[i], con);

        sched_sum(total, diff[i]);
        sched_sum(total_now, after[i]);
        total_tasks += tasks;
    }

    sched_line("all", total_tasks, total, total_now, con);

    mm::free(snaps);
}

/* Sampled on the per-CPU tick, so the resolution is the tick rate: enough
   to find where the time goes, not enough to see inside a short function. */
static void cmd_profile(const char* args, stdlib::Printer& con)
{
    static const ulong DEFAULT_MS = 1000;
    static const ulong MAX_MS = 2000;

    const char* end;
    const char* tok = stdlib::next_token(args, end);

    ulong ms = DEFAULT_MS;
    ulong pid = Profiler::NO_PID_FILTER;
    ulong chains = Profiler::TOP_CHAINS;

    /* Both consts: -Wformat reads a format through a pointer that cannot
       change */
    static const char* const USAGE =
        "usage: profile [ms, 1-%lu] [pid|all] [chains]\n";

    if (tok)
    {
        char buf[24];
        stdlib::token_copy(tok, end, buf, sizeof(buf));

        ulong value;
        if (!stdlib::parse_ulong(buf, value) || value == 0 || value > MAX_MS)
        {
            con.printf(USAGE, MAX_MS);
            return;
        }
        ms = value;

        tok = stdlib::next_token(end, end);
        if (tok)
        {
            stdlib::token_copy(tok, end, buf, sizeof(buf));

            /* `all` is how a chain count is given without a pid filter: the
               argument that matters on a console with no scrollback is the
               third one, and it should not require inventing a pid. */
            if (stdlib::str_cmp(buf, "all") != 0)
            {
                if (!stdlib::parse_ulong(buf, value))
                {
                    con.printf(USAGE, MAX_MS);
                    return;
                }
                pid = value;
            }

            tok = stdlib::next_token(end, end);
            if (tok)
            {
                stdlib::token_copy(tok, end, buf, sizeof(buf));
                if (!stdlib::parse_ulong(buf, value) || value == 0 ||
                    value > Profiler::TOP_CHAINS)
                {
                    con.printf(USAGE, MAX_MS);
                    return;
                }
                chains = value;
            }
        }
    }

    auto& profiler = Profiler::get_instance();
    if (!profiler.start())
    {
        con.printf("profile: could not start\n");
        return;
    }

    sleep(ms * consts::NANO_SECS_IN_MS);
    profiler.stop();

    con.printf("profiled %lu ms", ms);
    if (pid != Profiler::NO_PID_FILTER)
        con.printf(", pid %lu only", pid);
    con.printf("\n");

    profiler.report(con, pid, chains);
}

static void cmd_stacks(const char* args, stdlib::Printer& con)
{
    (void)args;

    /* High-water marks, not a snapshot: each stack was filled with a pattern
       when it was created, and what is still intact is what it never
       reached. A spike that lasted a microsecond during boot shows up here
       just as clearly as a steady load. */
    ulong worst_free = (ulong)-1;

    con.printf("kind id used size name\n");
    report_cpu_stacks(con, worst_free);
    TaskTable::get_instance().stacks(con, worst_free);

    if (worst_free == (ulong)-1)
    {
        con.printf("no stack has been touched yet\n");
        return;
    }

    con.printf("closest any stack came to its end: %lu bytes free\n", worst_free);
}


static void cmd_ps(const char* args, stdlib::Printer& con)
{
    (void)args;
    TaskTable::get_instance().ps(con);
}

static void cmd_watchdog(const char* args, stdlib::Printer& con)
{
    (void)args;
    Watchdog::get_instance().dump(con);
}

static void cmd_memusage(const char* args, stdlib::Printer& con)
{
    (void)args;
    auto& pt = mm::PageTable::get_instance();

    con.printf("free_pages: %lu\n", pt.get_free_pages_count());
    con.printf("totalPages: %lu\n", pt.get_total_pages_count());
    con.printf("hugePages: %lu free of %lu\n", pt.get_huge_pool_free(), pt.get_huge_pool_blocks());
}

static void cmd_memcheck(const char* args, stdlib::Printer& con)
{
    (void)args;
    mm::PageTable::get_instance().check_free_list(con);
}

static void cmd_meminfo(const char* args, stdlib::Printer& con)
{
    (void)args;
    auto& mmap = mm::MemoryMap::get_instance();
    auto& pt = mm::PageTable::get_instance();

    ulong usable_regions = 0;
    for (size_t i = 0; i < mmap.get_region_count(); i++)
    {
        ulong addr, len, type;
        if (!mmap.get_region(i, addr, len, type))
            break;

        con.printf("0x%lX 0x%lX %s\n", addr, len,
            mm::MemoryMap::get_region_type_name(type));

        if (type == mm::MemoryMap::USABLE_RAM_TYPE)
            usable_regions++;
    }

    /* The interesting number on a big machine is the third one: the kernel
       can only free-list RAM the bootstrap linear map reaches, and until
       that map grows, everything above it is memory the box has and the
       kernel does not know how to touch. */
    ulong map_limit = mm::BuiltinPageTable::get_instance().get_mapped_limit();
    ulong usable = mmap.get_usable_ram_bytes();
    ulong unused = mmap.get_usable_ram_bytes_above(map_limit);

    con.printf("usable: %lu MiB in %lu regions\n", usable / consts::MB, usable_regions);
    con.printf("mapped: %lu MiB, bootstrap map ends at 0x%lX\n",
        (usable - unused) / consts::MB, map_limit);
    if (unused != 0)
        con.printf("unused: %lu MiB above the bootstrap map\n", unused / consts::MB);
    con.printf("pages: %lu free of %lu\n",
        pt.get_free_pages_count(), pt.get_total_pages_count());
    /* The huge pool is neither: its pages left the free list at boot, for
       guests' memory alone (hugepages=). */
    if (pt.get_huge_pool_blocks() != 0)
        con.printf("huge pool: %lu 2MiB pages free of %lu (%lu MiB of %lu)\n",
            pt.get_huge_pool_free(), pt.get_huge_pool_blocks(),
            pt.get_huge_pool_free() * mm::PageTable::HUGE_FRAME_SIZE / consts::MB,
            pt.get_huge_pool_blocks() * mm::PageTable::HUGE_FRAME_SIZE / consts::MB);
}

static void cmd_irqstat(const char* args, stdlib::Printer& con)
{
    (void)args;
    for (ulong i = 0; i < InterruptStats::COUNT; i++)
    {
        InterruptSource src = (InterruptSource)i;
        long count = InterruptStats::get(src);
        if (count > 0)
            con.printf("%s: %lu\n", InterruptStats::get_name(src), count);
    }
}

static void cmd_pci(const char* args, stdlib::Printer& con)
{
    (void)args;
    Pci::get_instance().dump(con);
}


/* crc32 <path>: the CRC-32 of a file, to check a copy against the host
   (python3 -c "import zlib,sys; print(hex(zlib.crc32(open(sys.argv[1],'rb').read())))" file) */
static void cmd_crc32(const char* args, stdlib::Printer& con)
{
    static const ulong CHUNK_SIZE = 64 * 1024;

    const char* end;
    const char* path_start = stdlib::next_token(args, end);
    if (path_start == nullptr)
    {
        con.printf("usage: crc32 <path>\n");
        return;
    }
    char path[MAX_PATH];
    stdlib::token_copy(path_start, end, path, sizeof(path));

    RustFile* file = file_open(path, FILE_READ);
    if (file == nullptr)
    {
        con.printf("open failed\n");
        return;
    }

    u8* buf = (u8*)mm::alloc(CHUNK_SIZE, 0);
    if (buf == nullptr)
    {
        con.printf("alloc failed\n");
        kernel_vfs_close(file);
        return;
    }

    u32 crc = 0;
    ulong total = 0;
    bool ok = true;
    for (;;)
    {
        ulong got = 0;
        if (kernel_vfs_read(file, buf, CHUNK_SIZE, &got) != 0)
        {
            ok = false;
            break;
        }
        if (got == 0)
            break;
        crc = stdlib::crc32_update(crc, buf, got);
        total += got;
    }

    mm::free(buf);
    kernel_vfs_close(file);

    if (ok)
        con.printf("%s: crc32 0x%lX, %lu bytes\n", path, (ulong)crc, total);
    else
        con.printf("read failed\n");
}

/* A GRUB environment block bigger than this is not one grub-editenv made */
static const ulong GRUBENV_MAX_SIZE = 64 * 1024;

/* name=value with its terminator */
static const ulong GRUBENV_ASSIGNMENT_SIZE =
    stdlib::GrubEnvBlock::MAX_NAME_LEN + 1 + stdlib::GrubEnvBlock::MAX_VALUE_LEN + 1;

static bool grubenv_print_var(const char* name, const char* value, void* ctx)
{
    auto* con = static_cast<stdlib::Printer*>(ctx);
    con->printf("%s=%s\n", name, value);
    return true;
}

/* The whole file, or nullptr with the reason printed; size comes with it */
static char* grubenv_read_file(const char* path, ulong& size, stdlib::Printer& con)
{
    RustFile* file = file_open(path, FILE_READ);
    if (file == nullptr)
    {
        con.printf("open failed\n");
        return nullptr;
    }

    size = kernel_vfs_size(file);
    if (size < stdlib::GrubEnvBlock::MIN_SIZE || size > GRUBENV_MAX_SIZE)
    {
        con.printf("%s: %lu bytes is not a GRUB environment block\n", path, size);
        kernel_vfs_close(file);
        return nullptr;
    }

    char* block = (char*)mm::alloc(size, 0);
    if (block == nullptr)
    {
        con.printf("alloc failed\n");
        kernel_vfs_close(file);
        return nullptr;
    }

    ulong total = 0;
    while (total < size)
    {
        ulong got = 0;
        if (kernel_vfs_read(file, block + total, size - total, &got) != 0 || got == 0)
            break;
        total += got;
    }
    kernel_vfs_close(file);

    if (total != size)
    {
        con.printf("read failed\n");
        mm::free(block);
        return nullptr;
    }
    return block;
}

/* grubenv <path>: the variables in a GRUB environment block, as
   grub-editenv list prints them. grubenv <path> name=value ...: set them
   (name= with nothing after it removes one) and write the block back where
   it was. This is how a running kernel arms a one-shot boot for the next
   GRUB -- see docs/real-hardware.md. */
static void cmd_grubenv(const char* args, stdlib::Printer& con)
{
    const char* end;
    const char* path_start = stdlib::next_token(args, end);
    if (path_start == nullptr)
    {
        con.printf("usage: grubenv <path> [name=value ...]  (name= removes it)\n");
        return;
    }
    char path[MAX_PATH];
    stdlib::token_copy(path_start, end, path, sizeof(path));

    ulong size = 0;
    char* block = grubenv_read_file(path, size, con);
    if (block == nullptr)
        return;

    stdlib::GrubEnvBlock env(block, size);
    if (!env.is_valid())
    {
        con.printf("%s: not a GRUB environment block\n", path);
        mm::free(block);
        return;
    }

    const char* tok_start = stdlib::next_token(end, end);
    if (tok_start == nullptr)
    {
        if (!env.for_each(grubenv_print_var, &con))
            con.printf("%s: malformed from here on\n", path);
        mm::free(block);
        return;
    }

    ulong changes = 0;
    while (tok_start != nullptr)
    {
        char assignment[GRUBENV_ASSIGNMENT_SIZE];
        stdlib::token_copy(tok_start, end, assignment, sizeof(assignment));

        const char* sep = stdlib::str_chr_once(assignment, '=');
        if (sep == nullptr || sep == assignment)
        {
            con.printf("%s: expected name=value\n", assignment);
            mm::free(block);
            return;
        }
        assignment[sep - assignment] = '\0';
        const char* name = assignment;
        const char* value = sep + 1;

        if (*value == '\0')
        {
            if (env.unset(name))
            {
                con.printf("%s unset\n", name);
                changes++;
            }
            else
            {
                con.printf("%s was not set\n", name);
            }
        }
        else if (env.set(name, value))
        {
            con.printf("%s=%s\n", name, value);
            changes++;
        }
        else
        {
            con.printf("cannot set %s: not a name GRUB takes, or no room in %lu bytes\n",
                name, size);
            mm::free(block);
            return;
        }

        tok_start = stdlib::next_token(end, end);
    }

    if (changes == 0)
    {
        mm::free(block);
        return;
    }

    /* Back in place, at the same size: GRUB's save_env writes the file's own
       disk blocks, so the file has to keep them */
    RustFile* file = file_open(path, FILE_WRITE);
    bool ok = (file != nullptr) && kernel_vfs_write(file, block, size) == 0;
    if (file != nullptr)
        kernel_vfs_close(file);
    mm::free(block);

    if (!ok)
    {
        con.printf("%s: write failed\n", path);
        return;
    }
    if (kernel_vfs_sync() != 0)
        con.printf("sync failed\n");
}

static void cmd_version(const char* args, stdlib::Printer& con)
{
    (void)args;
    con.printf("nos %s (%s)\n", KERNEL_VERSION, KERNEL_GIT_REV);
}

static void dump_stack_trace(ulong* frames, size_t count, stdlib::Printer& con)
{
    auto& symtab = SymbolTable::get_instance();
    for (size_t i = 0; i < count; i++)
    {
        char where[SymbolTable::DESCRIBE_MAX];
        if (symtab.describe_return(frames[i], where, sizeof(where)))
            con.printf("  [%lu] 0x%lX %s\n", (ulong)i, frames[i], where);
        else
            con.printf("  [%lu] 0x%lX\n", (ulong)i, frames[i]);
    }
}

static void cmd_bt(const char* args, stdlib::Printer& con)
{
    const char* end;
    const char* pid_start = stdlib::next_token(args, end);
    if (!pid_start)
    {
        con.printf("usage: bt <pid>\n");
        return;
    }

    char pid_buf[16];
    stdlib::token_copy(pid_start, end, pid_buf, sizeof(pid_buf));

    ulong pid = 0;
    if (!stdlib::parse_ulong(pid_buf, pid))
    {
        con.printf("invalid pid\n");
        return;
    }

    ObjectPtr<Task> task(TaskTable::get_instance().lookup(pid));
    if (!task)
    {
        con.printf("task %lu not found\n", pid);
        return;
    }

    Task::Where where;
    task->locate(where);

    char what[2 * SymbolTable::DESCRIBE_MAX];
    where.describe(what, sizeof(what));
    con.printf("task %lu (%s) %s:\n", task->pid_, task->get_name(), what);
    dump_stack_trace(where.frames, where.count, con);
}

/* A stack that is no task's, for `panic offstack`: the panic path from
   where there is no task to ask about -- x86's double fault's stack, a
   parked CPU's -- with preemption on, as a running system has it. */
static char off_stack[4 * consts::PAGE_SIZE] __attribute__((aligned(16)));

static void panic_off_stack(void* ctx)
{
    (void)ctx;
    PANIC("user requested panic, off a task's stack");
}

static void cmd_panic(const char* args, stdlib::Printer& con)
{
    const char* end;
    const char* type_start = stdlib::next_token(args, end);

    if (!type_start)
    {
        PANIC("user requested panic");
    }

    char type[16];
    stdlib::token_copy(type_start, end, type, sizeof(type));

    if (stdlib::str_cmp(type, "pf") == 0)
    {
        con.printf("triggering page fault...\n");
        volatile int* p = nullptr;
        // cppcheck-suppress nullPointer
        *p = 0;
    }
    else if (stdlib::str_cmp(type, "div0") == 0)
    {
        con.printf("triggering divide by zero...\n");
        volatile int zero = 0;
        // cppcheck-suppress zerodiv
        volatile int x = 1 / zero;
        (void)x;
    }
    else if (stdlib::str_cmp(type, "ud") == 0)
    {
        con.printf("triggering invalid opcode...\n");
        hal::undef_instr();
    }
    else if (stdlib::str_cmp(type, "df") == 0)
    {
        con.printf("triggering double fault...\n");
        if (!hal::double_fault())
            con.printf("no double fault here: an exception has no stack of its own on this architecture\n");
    }
    else if (stdlib::str_cmp(type, "offstack") == 0)
    {
        con.printf("panicking on a stack that is no task's...\n");
        interrupt_disable();
        hal::run_on_stack((ulong)&off_stack[sizeof(off_stack)], panic_off_stack, nullptr);
    }
    else
    {
        con.printf("usage: panic [pf|div0|ud|df|offstack]\n");
        con.printf("  (no arg) - direct panic\n");
        con.printf("  pf       - page fault (null deref)\n");
        con.printf("  div0     - divide by zero\n");
        con.printf("  ud       - invalid opcode\n");
        con.printf("  df       - double fault, on its own stack (x86-64)\n");
        con.printf("  offstack - panic on a stack that is no task's\n");
    }
}

/* Each in a task of its own, which the shell waits for only so long: an
   unload waits out a module command still running, however long it runs */
static void cmd_insmod(const char* args, stdlib::Printer& con)
{
    if (args[0] == '\0')
    {
        con.printf("usage: insmod <path> [parameters]\n");
        return;
    }

    ModuleTable::get_instance().start_load(args, con);
}

static void cmd_rmmod(const char* args, stdlib::Printer& con)
{
    if (args[0] == '\0')
    {
        con.printf("usage: rmmod <name>\n");
        return;
    }

    ModuleTable::get_instance().start_unload(args, con);
}

static void cmd_lsmod(const char* args, stdlib::Printer& con)
{
    (void)args;
    ModuleTable::get_instance().dump(con);
}

/* /etc/rc: shell commands run once at boot, after the network is set up --
   what loads and starts a module like sshd on a machine nobody can reach
   until it has. `rc` shows and edits it; rc=off on the kernel command line
   skips it at boot. */
static const char RC_PATH[] = "/etc/rc";
static const char RC_DIR[] = "/etc";
/* The most of a script read, and the longest line run: long enough for a
   guest's `hv start` with a distribution's kernel command line, which runs
   past two hundred characters before it says anything of its own. Lines
   are copied through a buffer this long on the stack of the task that runs
   the script, a kernel task's 64 KiB. */
static const ulong SCRIPT_SIZE_MAX = 16 * consts::KB;
static const ulong SCRIPT_LINE_MAX = 1023;
static const ulong SCRIPT_TAG = 'Rc  ';

/* A script, NUL-terminated, in a buffer from mm::alloc the caller frees --
   or nullptr, said on out, when it cannot be read. Read through the
   whole-file calls, which look for it where a cut-short write may have left
   it as well as where it belongs. Past SCRIPT_SIZE_MAX only its whole lines
   are taken, whole comes back false, and it is said: a command cut at the
   limit would run as some other command. */
static char* read_script(const char* path, ulong& size, bool& whole, stdlib::Printer& out)
{
    const ulong path_len = stdlib::str_len(path);
    const long total = kernel_file_size(path, path_len);
    if (total < 0)
    {
        out.printf("rc: cannot open %s\n", path);
        return nullptr;
    }

    char* text = static_cast<char*>(mm::alloc(SCRIPT_SIZE_MAX + 1, SCRIPT_TAG));
    if (text == nullptr)
    {
        out.printf("rc: no memory to read %s\n", path);
        return nullptr;
    }

    const long got = kernel_file_read(path, path_len, text, SCRIPT_SIZE_MAX);
    if (got < 0)
    {
        mm::free(text);
        out.printf("rc: cannot read %s\n", path);
        return nullptr;
    }
    size = (ulong)got;
    whole = ((ulong)total <= size);
    if (!whole)
    {
        ulong keep = size;
        while (keep > 0 && text[keep - 1] != '\n')
            keep--;
        out.printf("rc: %s is %lu bytes; only its whole lines in the first %lu are read\n",
            path, total, size);
        size = keep;
    }

    text[size] = '\0';
    return text;
}

/* The line of text at p, blanks and a CR trimmed off either end, with p
   moved past it; false at the end of the text */
static bool next_script_line(const char*& p, const char*& line, ulong& len)
{
    if (*p == '\0')
        return false;

    const char* start = p;
    while (*p != '\0' && *p != '\n')
        p++;
    const char* end = p;
    if (*p == '\n')
        p++;

    while (start < end && (*start == ' ' || *start == '\t'))
        start++;
    while (end > start && (end[-1] == ' ' || end[-1] == '\t' || end[-1] == '\r'))
        end--;

    line = start;
    len = static_cast<ulong>(end - start);
    return true;
}

/* A Printer into the kernel log, for what /etc/rc prints at boot: on a
   machine whose console is the network nobody sits at a console, and the
   netconsole and dmesg are where it can be read. What a command prints
   waits in Buf and goes to the log a line at a time once the command has
   returned (Commit), not as it is printed: a command that reads the log --
   dmesg -- would read its own output back as it went. The shell has taken
   the console from the log by then, so it goes to echo as well, as it is
   printed -- the screen of a machine that has one. */
class LogPrinter final : public stdlib::Printer
{
public:
    LogPrinter(const char* prefix, stdlib::Printer* echo, char* buf, ulong size)
        : prefix_(prefix)
        , echo_(echo)
        , buf_(buf)
        , size_(size)
        , len_(0)
        , dropped_(0)
    {
    }

    virtual PRINTF_FORMAT(2, 3) void printf(const char *fmt, ...) override
    {
        va_list args;
        va_start(args, fmt);
        v_printf(fmt, args);
        va_end(args);
    }

    virtual PRINTF_FORMAT(2, 0) void v_printf(const char *fmt, va_list args) override
    {
        char text[FORMAT_MAX];
        if (stdlib::vsn_printf(text, sizeof(text), fmt, args) < 0)
            return;
        add(text);
    }

    virtual void print_string(const char *s) override
    {
        if (s != nullptr)
            add(s);
    }

    virtual void backspace() override
    {
    }

    /* A command is done: what it printed goes to the log, a line at a time,
       a line longer than the log takes in pieces */
    void commit()
    {
        ulong start = 0;
        while (start < len_)
        {
            ulong end = start;
            while (end < len_ && buf_[end] != '\n' && end - start < LINE_LEN_MAX)
                end++;

            char line[LINE_LEN_MAX + 1];
            ulong n = 0;
            for (ulong i = start; i < end; i++)
            {
                if (buf_[i] != '\r')
                    line[n++] = buf_[i];
            }
            line[n] = '\0';
            if (n != 0)
                trace(0, "%s%s", prefix_, line);

            start = (end < len_ && buf_[end] == '\n') ? end + 1 : end;
        }
        if (dropped_ != 0)
            trace(0, "%s[%lu more bytes it printed not logged]", prefix_, dropped_);

        len_ = 0;
        dropped_ = 0;
    }

private:
    LogPrinter(const LogPrinter& other) = delete;
    LogPrinter& operator=(const LogPrinter& other) = delete;

    void add(const char* s)
    {
        if (echo_ != nullptr)
            echo_->print_string(s);

        for (; *s != '\0'; s++)
        {
            if (len_ == size_)
                dropped_++;
            else
                buf_[len_++] = *s;
        }
    }

    /* Room, in the trace line's 256 bytes, for its own prefix */
    static const ulong LINE_LEN_MAX = 160;
    static const ulong FORMAT_MAX = 512;

    const char* prefix_;
    stdlib::Printer* echo_;
    char* buf_;
    ulong size_;
    ulong len_;
    ulong dropped_;
};

/* What a boot script command printed, into the log: run_script's step */
static void commit_log(void* ctx)
{
    static_cast<LogPrinter*>(ctx)->commit();
}

/* What of a boot script command's output the log takes */
static const ulong RC_LOG_SIZE = 8 * consts::KB;

static void rc_show(stdlib::Printer& con)
{
    if (kernel_file_size(RC_PATH, stdlib::str_len(RC_PATH)) < 0)
    {
        con.printf("rc: no %s -- rc add <command line> makes one\n", RC_PATH);
        return;
    }

    ulong size = 0;
    bool whole = true;
    char* text = read_script(RC_PATH, size, whole, con);
    if (text == nullptr)
        return;

    const char* p = text;
    const char* line = nullptr;
    ulong len = 0;
    ulong number = 0;
    char shown[SCRIPT_LINE_MAX + 1];
    while (next_script_line(p, line, len))
    {
        number++;
        ulong n = (len < SCRIPT_LINE_MAX) ? len : SCRIPT_LINE_MAX;
        stdlib::mem_cpy(shown, line, n);
        shown[n] = '\0';
        con.printf("%lu  %s\n", number, shown);
    }
    if (number == 0)
        con.printf("rc: %s is empty\n", RC_PATH);

    mm::free(text);
}

/* Writes a new /etc/rc: the lines of the old one but the one numbered skip
   (0: none), then add if there is one -- through kernel_file_write, since
   what it is for is the next boot, and a full disk must not leave it empty.
   One edit at a time (rc_lock_): two at once would each write over the other
   one's line. */
static bool rc_rewrite(ulong skip, const char* add, stdlib::Printer& con)
{
    stdlib::AutoLock lock(Cmd::get_instance().get_rc_lock());

    ulong size = 0;
    char* old = nullptr;
    if (kernel_file_size(RC_PATH, stdlib::str_len(RC_PATH)) >= 0)
    {
        bool whole = true;
        old = read_script(RC_PATH, size, whole, con);
        if (old == nullptr)
            return false;
        if (!whole)
        {
            mm::free(old);
            con.printf("rc: %s is too large to edit with rc\n", RC_PATH);
            return false;
        }
    }
    else if (skip != 0)
    {
        con.printf("rc: no %s\n", RC_PATH);
        return false;
    }
    else if (kernel_dir_create(RC_DIR, stdlib::str_len(RC_DIR)) != 0)
    {
        con.printf("rc: cannot make %s\n", RC_DIR);
        return false;
    }

    if (skip != 0)
    {
        const char* p = old;
        const char* line = nullptr;
        ulong len = 0;
        ulong lines = 0;
        while (next_script_line(p, line, len))
            lines++;
        if (skip > lines)
        {
            mm::free(old);
            con.printf("rc: %s has no line %lu\n", RC_PATH, skip);
            return false;
        }
    }

    ulong add_len = (add != nullptr) ? stdlib::str_len(add) : 0;
    if (size + add_len + 1 > SCRIPT_SIZE_MAX)
    {
        if (old != nullptr)
            mm::free(old);
        con.printf("rc: %s would pass %lu bytes\n", RC_PATH, SCRIPT_SIZE_MAX);
        return false;
    }

    char* text = static_cast<char*>(mm::alloc(SCRIPT_SIZE_MAX + 1, SCRIPT_TAG));
    if (text == nullptr)
    {
        if (old != nullptr)
            mm::free(old);
        con.printf("rc: no memory\n");
        return false;
    }

    /* Trimmed as the lines are, every one ends up with its newline */
    ulong pos = 0;
    ulong number = 0;
    if (old != nullptr)
    {
        const char* p = old;
        const char* line = nullptr;
        ulong len = 0;
        while (next_script_line(p, line, len))
        {
            number++;
            if (number == skip)
                continue;
            stdlib::mem_cpy(text + pos, line, len);
            pos += len;
            text[pos++] = '\n';
        }
        mm::free(old);
    }
    if (add_len != 0)
    {
        stdlib::mem_cpy(text + pos, add, add_len);
        pos += add_len;
        text[pos++] = '\n';
    }

    bool ok = kernel_file_write(RC_PATH, stdlib::str_len(RC_PATH), text, pos) == 0;
    mm::free(text);
    if (!ok)
    {
        con.printf("rc: cannot write %s\n", RC_PATH);
        return false;
    }
    return true;
}

static void cmd_rc(const char* args, stdlib::Printer& con)
{
    const char* end;
    const char* word = stdlib::next_token(args, end);
    if (word == nullptr)
    {
        rc_show(con);
        return;
    }

    char verb[8];
    stdlib::token_copy(word, end, verb, sizeof(verb));

    const char* rest = end;
    while (*rest == ' ')
        rest++;

    if (stdlib::str_cmp(verb, "add") == 0)
    {
        ulong len = stdlib::str_len(rest);
        if (len == 0)
            con.printf("usage: rc add <command line>\n");
        else if (len > SCRIPT_LINE_MAX)
            con.printf("rc: at most %lu characters a line\n", SCRIPT_LINE_MAX);
        else if (rc_rewrite(0, rest, con))
            con.printf("rc: added to %s: %s\n", RC_PATH, rest);
    }
    else if (stdlib::str_cmp(verb, "del") == 0)
    {
        ulong number = 0;
        if (!stdlib::parse_ulong(rest, number) || number == 0)
            con.printf("usage: rc del <n> -- n as rc numbers the lines\n");
        else if (rc_rewrite(number, nullptr, con))
            rc_show(con);
    }
    else if (stdlib::str_cmp(verb, "clear") == 0)
    {
        stdlib::AutoLock lock(Cmd::get_instance().get_rc_lock());
        const ulong path_len = stdlib::str_len(RC_PATH);
        if (kernel_file_size(RC_PATH, path_len) < 0)
        {
            con.printf("rc: no %s\n", RC_PATH);
        }
        else if (kernel_file_remove(RC_PATH, path_len) == 0)
        {
            /* Both of the pair go, should a cut-short edit have left both */
            con.printf("rc: removed %s\n", RC_PATH);
        }
        else
        {
            con.printf("rc: cannot remove %s\n", RC_PATH);
        }
    }
    else if (stdlib::str_cmp(verb, "run") == 0)
    {
        Cmd::get_instance().run_script(RC_PATH, con);
    }
    else
    {
        con.printf("usage: rc [add <command line> | del <n> | clear | run]\n");
    }
}

// Forward declaration - cmd_help needs the Commands array defined below
static void cmd_help(const char* args, stdlib::Printer& con);

static const CmdEntry COMMANDS[] = {
    { "cls",       cmd_cls,       "cls - clear screen" },
    { "cpu",       cmd_cpu,       "cpu - dump cpu state" },
    { "lscpu",     cmd_lscpu,     "lscpu - identify the cpu and the features the kernel needs" },
    { "dmesg",     cmd_dmesg,     "dmesg [lines] [filter] - dump kernel log" },
    { "loglevel",  cmd_loglevel,  "loglevel [N] - show or set trace level" },
    { "uptime",    cmd_uptime,    "uptime - show uptime" },
    { "date",      cmd_date,      "date - show wall clock time" },
    { "ps",        cmd_ps,        "ps - show tasks" },
    { "stacks",    cmd_stacks,    "stacks - stack high-water marks" },
    { "top",       cmd_top,       "top [ms] - per-task cpu use over a sampling window" },
    { "sched",     cmd_sched,     "sched [ms] | sched probe [spinners=N] [ms=M] [cpu=N] | sched queue [cpu] | sched nice <pid> <n> | sched slice <pid> <us> - what each cpu's scheduler did over a window; how long a woken task waits behind spinners; what EEVDF has of a cpu's tasks; a task's weight and slice" },
    { "profile",   cmd_profile,   "profile [ms] [pid] - sample where the kernel spends its time" },
    { "watchdog",  cmd_watchdog,  "watchdog - show watchdog stats" },
    { "memusage",  cmd_memusage,  "memusage - show memory usage stats" },
    { "meminfo",   cmd_meminfo,   "meminfo - show the firmware memory map and what of it is used" },
    { "memcheck",  cmd_memcheck,  "memcheck - verify no reserved page reached the free list" },
    { "irqstat",   cmd_irqstat,   "irqstat - show interrupt statistics" },
    { "pci",       cmd_pci,       "pci - show pci devices" },
    { "crc32",     cmd_crc32,     "crc32 <path> - CRC-32 of a file" },
    { "grubenv",   cmd_grubenv,   "grubenv <path> [name=value ...] - show or set GRUB environment variables" },
    { "insmod",    cmd_insmod,    "insmod <path> [params] - load a kernel module (.ko), handing its init the parameters" },
    { "rmmod",     cmd_rmmod,     "rmmod <name> - unload a kernel module" },
    { "lsmod",     cmd_lsmod,     "lsmod - list the loaded kernel modules" },
    { "rc",        cmd_rc,        "rc [add <command line>|del <n>|clear|run] - show or edit /etc/rc, run at boot" },
    { "version",   cmd_version,   "version - show kernel version" },
    { "bt",        cmd_bt,        "bt <pid> - show task backtrace" },
    { "panic",     cmd_panic,     "panic [pf|div0|ud|df|offstack] - trigger kernel panic" },
    { "poweroff",  cmd_poweroff,  "poweroff - power off (ACPI S5)" },
    { "shutdown",  cmd_poweroff,  nullptr },
    { "reboot",    cmd_reboot,    "reboot - reset system" },
    { "help",      cmd_help,      "help - help" },
    { nullptr,     nullptr,      nullptr },
};

static void cmd_help(const char* args, stdlib::Printer& con)
{
    (void)args;
    for (ulong i = 0; COMMANDS[i].name != nullptr; i++)
    {
        if (COMMANDS[i].help != nullptr)
            con.printf("%s\n", COMMANDS[i].help);
    }

    Cmd::get_instance().dynamic_help(con);
}

Cmd::Cmd()
    : task_ptr_(nullptr)
    , shutdown_(false)
    , reboot_(false)
    , active_(false)
    , script_running_(false)
    , dynamic_generation_(0)
{
    stdlib::mem_set(dynamic_, 0, sizeof(dynamic_));
}

Cmd::~Cmd()
{
    if (task_ptr_ != nullptr)
    {
        task_ptr_->put();
        task_ptr_ = nullptr;
    }
}

void Cmd::dispatch(const char *cmd, stdlib::Printer& out)
{
    bool found = false;
    for (ulong i = 0; COMMANDS[i].name != nullptr; i++)
    {
        ulong name_len = stdlib::str_len(COMMANDS[i].name);
        if (stdlib::str_cmp(cmd, COMMANDS[i].name) == 0)
        {
            COMMANDS[i].handler("", out);
            found = true;
            break;
        }
        else if (stdlib::mem_cmp(cmd, COMMANDS[i].name, name_len) == 0 && cmd[name_len] == ' ')
        {
            COMMANDS[i].handler(cmd + name_len + 1, out);
            found = true;
            break;
        }
    }

    if (!found)
        found = get_instance().dispatch_dynamic(cmd, out);

    if (!found)
        out.printf("command '%s' not found\n", cmd);
}

bool Cmd::dispatch_dynamic(const char* cmd, stdlib::Printer& out)
{
    DynamicHandler handler = nullptr;
    void* ctx = nullptr;
    const char* args = "";
    ulong slot = 0;

    {
        stdlib::AutoLock lock(dynamic_lock_);
        for (ulong i = 0; i < DYNAMIC_MAX; i++)
        {
            DynamicCmd& entry = dynamic_[i];
            if (entry.handle == 0 || entry.removing)
                continue;

            ulong name_len = stdlib::str_len(entry.name);
            if (stdlib::str_cmp(cmd, entry.name) == 0)
                args = "";
            else if (stdlib::mem_cmp(cmd, entry.name, name_len) == 0 && cmd[name_len] == ' ')
                args = cmd + name_len + 1;
            else
                continue;

            entry.running++;
            handler = entry.handler;
            ctx = entry.ctx;
            slot = i;
            break;
        }
    }

    if (handler == nullptr)
        return false;

    /* Without the lock: the handler is the module's code and may take as
       long as it likes. Running keeps unregister_dynamic -- and with it the
       module's unload -- waiting until it returns. */
    handler(ctx, args, stdlib::str_len(args), &out);

    stdlib::AutoLock lock(dynamic_lock_);
    dynamic_[slot].running--;
    return true;
}

ulong Cmd::register_dynamic(const char* name, ulong name_len, const char* help, ulong help_len,
    DynamicHandler handler, void* ctx)
{
    if (name == nullptr || handler == nullptr || name_len == 0 || name_len > DYNAMIC_NAME_MAX)
        return 0;

    char key[DYNAMIC_NAME_MAX + 1];
    for (ulong i = 0; i < name_len; i++)
    {
        if (name[i] <= ' ' || name[i] > '~')
            return 0;
        key[i] = name[i];
    }
    key[name_len] = '\0';

    for (ulong i = 0; COMMANDS[i].name != nullptr; i++)
    {
        if (stdlib::str_cmp(COMMANDS[i].name, key) == 0)
            return 0;
    }

    stdlib::AutoLock lock(dynamic_lock_);

    DynamicCmd* entry = nullptr;
    ulong slot = 0;
    for (ulong i = 0; i < DYNAMIC_MAX; i++)
    {
        if (dynamic_[i].handle == 0)
        {
            if (entry == nullptr)
            {
                entry = &dynamic_[i];
                slot = i;
            }
        }
        else if (stdlib::str_cmp(dynamic_[i].name, key) == 0)
        {
            return 0;
        }
    }

    if (entry == nullptr)
        return 0;

    ulong help_copy = (help != nullptr) ? help_len : 0;
    if (help_copy > DYNAMIC_HELP_MAX)
        help_copy = DYNAMIC_HELP_MAX;
    for (ulong i = 0; i < help_copy; i++)
        entry->help[i] = (help[i] >= ' ' && help[i] <= '~') ? help[i] : '?';
    entry->help[help_copy] = '\0';

    stdlib::mem_cpy(entry->name, key, name_len + 1);
    entry->handler = handler;
    entry->ctx = ctx;
    entry->running = 0;
    entry->removing = false;
    dynamic_generation_++;
    entry->handle = (dynamic_generation_ << DYNAMIC_SLOT_BITS) | (slot + 1);
    return entry->handle;
}

void Cmd::unregister_dynamic(ulong handle)
{
    const ulong index = handle & ((1UL << DYNAMIC_SLOT_BITS) - 1);
    if (index == 0 || index > DYNAMIC_MAX)
        return;

    DynamicCmd& entry = dynamic_[index - 1];
    {
        stdlib::AutoLock lock(dynamic_lock_);
        if (entry.handle != handle)
            return;
        entry.removing = true;
    }

    /* Wait out the calls already made: the module may free the handler's ctx,
       and unload its code, the moment this returns */
    for (;;)
    {
        {
            stdlib::AutoLock lock(dynamic_lock_);

            /* A second unregister of the same handle may have finished first
               and the slot found a new user since: not ours to clear */
            if (entry.handle != handle)
                return;

            if (entry.running == 0)
            {
                entry.handle = 0;
                return;
            }
        }
        sleep(DYNAMIC_POLL_NS);
    }
}

void Cmd::dynamic_help(stdlib::Printer& out)
{
    for (ulong i = 0; i < DYNAMIC_MAX; i++)
    {
        char help[DYNAMIC_HELP_MAX + 1];
        help[0] = '\0';
        {
            stdlib::AutoLock lock(dynamic_lock_);
            const DynamicCmd& entry = dynamic_[i];
            if (entry.handle != 0 && !entry.removing)
            {
                const char* text = (entry.help[0] != '\0') ? entry.help : entry.name;
                ulong len = stdlib::str_len(text);
                if (len > DYNAMIC_HELP_MAX)
                    len = DYNAMIC_HELP_MAX;
                stdlib::mem_cpy(help, text, len);
                help[len] = '\0';
            }
        }

        /* Printed without the lock: a console line can take a while */
        if (help[0] != '\0')
            out.printf("%s\n", help);
    }
}

bool Cmd::run_script(const char* path, stdlib::Printer& out, ScriptStep step, void* step_ctx)
{
    bool running;
    {
        stdlib::AutoLock lock(lock_);
        running = script_running_;
        script_running_ = true;
    }
    /* Said out of the lock, which keeps interrupts off: out may be an SSH
       session's, and what is printed there may wait for the network */
    if (running)
    {
        out.printf("rc: a script is running already -- one that ran itself would never end\n");
        return false;
    }

    ulong size = 0;
    bool whole = true;
    char* text = read_script(path, size, whole, out);
    bool ok = (text != nullptr);
    if (ok)
    {
        const char* p = text;
        const char* line = nullptr;
        ulong len = 0;
        ulong number = 0;
        char cmd[SCRIPT_LINE_MAX + 1];
        while (next_script_line(p, line, len))
        {
            number++;
            if (len == 0 || line[0] == '#')
                continue;
            if (len > SCRIPT_LINE_MAX)
            {
                out.printf("rc: line %lu is longer than %lu characters, skipped\n", number, SCRIPT_LINE_MAX);
                continue;
            }
            stdlib::mem_cpy(cmd, line, len);
            cmd[len] = '\0';
            out.printf("> %s\n", cmd);
            dispatch(cmd, out);
            if (step != nullptr)
                step(step_ctx);
        }
        mm::free(text);
    }

    {
        stdlib::AutoLock lock(lock_);
        script_running_ = false;
    }
    return ok;
}

void Cmd::run_boot_script()
{
    if (kernel_file_size(RC_PATH, stdlib::str_len(RC_PATH)) < 0)
        return;

    if (Parameters::get_instance().is_rc_off())
    {
        trace(0, "rc: %s skipped, rc=off", RC_PATH);
        return;
    }

    /* With no room to keep it, what the commands print is on the screen
       alone, and the log says how much */
    char* buf = static_cast<char*>(mm::alloc(RC_LOG_SIZE, SCRIPT_TAG));

    trace(0, "rc: running %s", RC_PATH);
    LogPrinter log("rc: ", &Console::get_instance(), buf, (buf != nullptr) ? RC_LOG_SIZE : 0);
    run_script(RC_PATH, log, commit_log, &log);
    log.commit();
    trace(0, "rc: %s done", RC_PATH);

    if (buf != nullptr)
        mm::free(buf);
}

/* The flags below are polled by the BSP's idle task, between halts. The
   scheduler runs an idle task only when nothing else on its queue can run
   -- and on a CPU carrying the shell, DHCP and USB poll tasks that could be
   never (a task that slept used to yield rather than block, and one that
   polls still does): the request would sit unseen for good. Let that task
   take its turn as an ordinary one from here on; it has nothing left to do
   but notice. */
static void wake_shutdown_watch()
{
    auto& cpus = CpuTable::get_instance();
    Task* idle = cpus.get_cpu(cpus.get_bsp_index()).get_idle_task();
    if (idle != nullptr)
        idle->clear_idle();
}

void Cmd::request_shutdown()
{
    shutdown_ = true;
    wake_shutdown_watch();
}

void Cmd::request_reboot()
{
    reboot_ = true;
    wake_shutdown_watch();
}

bool Cmd::should_shutdown()
{
    return shutdown_;
}

bool Cmd::should_reboot()
{
    return reboot_;
}

void Cmd::stop()
{
    if (active_)
    {
        BUG_ON(task_ptr_ == nullptr);
        active_ = false;
        task_ptr_->set_stopping();
        task_ptr_->wait();
    }
}

void Cmd::stop_dhcp()
{
    rust_net_dhcp_stop();
}

bool Cmd::start()
{
    if (task_ptr_ != nullptr)
        return false;

    auto task = mm::talloc<Task, TAG>("cmd");
    if (task == nullptr)
        return false;

    {
        stdlib::AutoLock lock(lock_);
        if (task_ptr_ == nullptr)
        {
            task_ptr_ = task;
        }
    }

    if (task_ptr_ != task)
    {
        task->put();
        return false;
    }

    if (!task_ptr_->start(&Cmd::run_func, this))
    {
        {
            stdlib::AutoLock lock(lock_);
            task = task_ptr_;
            task_ptr_ = nullptr;
        }
        task->put();
        return false;
    }

    {
        stdlib::AutoLock lock(lock_);
        active_ = true;
    }
    return true;
}

void Cmd::show_banner(stdlib::Printer& out)
{
    out.printf("\n");
    out.printf("  _   _  ___  ____\n");
    out.printf(" | \\ | |/ _ \\/ ___|\n");
    out.printf(" |  \\| | | | \\___ \\\n");
    out.printf(" | |\\  | |_| |___) |\n");
    out.printf(" |_| \\_|\\___/|____/\n");
    out.printf("\n");
}

void Cmd::run()
{
    auto& con = Console::get_instance();

    /* Wait for startup trace output to settle before suppressing console */
    sleep(100 * consts::NANO_SECS_IN_MS);

    Tracer::get_instance().set_console_suppressed(true);

    show_banner(con);

    if (Parameters::get_instance().is_dhcp_auto())
        rust_net_dhcp_auto(&con);

    run_boot_script();

    editor_.prompt(con);

    while (!Task::get_current_task()->is_stopping())
    {
        char c = 0;
        bool has_key = false;
        {
            stdlib::AutoLock lock(lock_);
            if (!buf_.is_empty())
            {
                c = buf_.get();
                has_key = true;
            }
        }

        /* Everything typed is taken before the task sleeps again: an arrow
           is three bytes, and a line may be pasted */
        if (!has_key)
        {
            sleep(10 * consts::NANO_SECS_IN_MS);
            continue;
        }

        if (editor_.key(c, con))
        {
            /* A line of nothing but spaces is a new prompt, as over SSH */
            const char* line = editor_.get_line();
            const char* word = line;
            while (*word == ' ')
                word++;
            if (*word != '\0')
                dispatch(line, con);
            editor_.prompt(con);
        }
    }

    Tracer::get_instance().set_console_suppressed(false);
}

void Cmd::run_func(void *ctx)
{
    Cmd* cmd = static_cast<Cmd*>(ctx);
    cmd->run();
}

void Cmd::on_char(char c, u8 code)
{
    /* The editor reads the character: the keyboards send a terminal's
       bytes, the serial ports are one */
    (void)code;

    stdlib::AutoLock lock(lock_);
    if (!active_)
        return;

    if (!buf_.put(c))
    {
        trace(0, "Can't save char");
        return;
    }
}

}
