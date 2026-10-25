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

/* The network layer is Rust (src/rust/net); its commands register
   themselves. These two are boot's, run by the shell's own task. */
extern "C" {
void rust_net_dhcp_auto(void* printer);
void rust_net_dhcp_stop();
}

/* The filesystem layer is Rust (src/rust/fs). These are the calls the shell
   makes on it -- a script to read and rewrite, a file to checksum, a download
   to write -- taking paths as bytes and a length rather than a C string.

   kernel_file_* are the whole-file calls, and they know about the pair a
   write leaves behind: a content written through kernel_file_write goes to
   <path>.new first and takes the old file's place only once it is whole on
   disk, and the reads below look in both places. */
extern "C" {

struct RustFile;

/* Open flags (crate::vfs) */
static const ulong FileRead = 1;
static const ulong FileWrite = 2;

RustFile* kernel_vfs_open(const char* path, ulong len, ulong flags);
void kernel_vfs_close(RustFile* file);
int kernel_vfs_read(RustFile* file, void* buf, ulong len, ulong* out);
int kernel_vfs_write(RustFile* file, const void* data, ulong len);
ulong kernel_vfs_size(RustFile* file);
int kernel_vfs_remove(const char* path, ulong len);
int kernel_vfs_sync();

long kernel_file_size(const char* path, ulong len);
long kernel_file_read(const char* path, ulong len, void* buf, ulong cap);
int kernel_file_write(const char* path, ulong len, const void* data, ulong dataLen);
int kernel_file_remove(const char* path, ulong len);
int kernel_dir_create(const char* path, ulong len);

}

namespace Kernel
{

/* What a path fits in, NUL included (crate::vfs::MAX_PATH) */
static const ulong MaxPath = 256;

/* The shell holds paths as C strings; the layer takes bytes and a length. */
static RustFile* FileOpen(const char* path, ulong flags)
{
    return kernel_vfs_open(path, Stdlib::StrLen(path), flags);
}


struct CmdEntry
{
    const char* Name;
    void (*Handler)(const char* args, Stdlib::Printer& con);
    const char* Help;
};

static void CmdCls(const char* args, Stdlib::Printer& con)
{
    (void)args;
    Console::GetInstance().Cls();
    (void)con;
}

static void CmdPoweroff(const char* args, Stdlib::Printer& con)
{
    (void)args;
    con.Printf("shutting down...\n");
    Cmd::GetInstance().RequestShutdown();
}

static void CmdReboot(const char* args, Stdlib::Printer& con)
{
    (void)args;
    con.Printf("rebooting...\n");
    Cmd::GetInstance().RequestReboot();
}

static void CmdLscpu(const char* args, Stdlib::Printer& con)
{
    (void)args;

    auto& cpus = CpuTable::GetInstance();
    ulong mask = cpus.GetRunningCpus();

    ulong count = 0;
    for (ulong i = 0; i < MaxCpus; i++)
    {
        if (mask & (1UL << i))
            count++;
    }

    con.Printf("cpus %lu running, id mask 0x%lX, bsp %lu\n",
        count, mask, cpus.GetBspIndexNoLock());

    Hal::PrintCpuInfo(con);
}

static void CmdCpu(const char* args, Stdlib::Printer& con)
{
    (void)args;
    Hal::PrintCpuState(con);
}

static void CmdDmesg(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* tok = Stdlib::NextToken(args, end);

    char filter[64];
    filter[0] = '\0';
    if (tok)
        Stdlib::TokenCopy(tok, end, filter, sizeof(filter));

    /* A numeric first argument is a line count, not a filter: "dmesg 40" is
       the newest 40 lines, "dmesg nvme" filters the whole log, "dmesg 40 nvme"
       does both. The count is what makes the command usable over the remote
       shell, whose reply buffer holds about forty lines and drops the rest --
       so without it every dmesg returns the first minute of boot. */
    ulong lastLines = 0;
    ulong count;
    if (filter[0] != '\0' && Stdlib::ParseUlong(filter, count))
    {
        lastLines = count;
        filter[0] = '\0';

        tok = Stdlib::NextToken(end, end);
        if (tok)
            Stdlib::TokenCopy(tok, end, filter, sizeof(filter));
    }

    Dmesg::GetInstance().Dump(con, lastLines,
                              (filter[0] != '\0') ? filter : nullptr);
}

static void CmdLoglevel(const char* args, Stdlib::Printer& con)
{
    auto& tracer = Tracer::GetInstance();

    const char* end;
    const char* tok = Stdlib::NextToken(args, end);
    if (!tok)
    {
        con.Printf("loglevel %lu\n", (ulong)tracer.GetLevel());
        return;
    }

    char buf[16];
    Stdlib::TokenCopy(tok, end, buf, sizeof(buf));

    ulong level;
    if (!Stdlib::ParseUlong(buf, level) || level > (ulong)MaxTraceLevel)
    {
        con.Printf("usage: loglevel [0-%lu]\n", (ulong)MaxTraceLevel);
        return;
    }

    /* The per-subsystem levels in trace.h are compile-time constants, so the
       only way to see what xHCI or the page allocator is doing used to be a
       rebuild and a reflash -- on a machine whose console is a UDP socket that
       is most of an afternoon. Raising the level is loud: level 4 traces every
       allocation, and every line also goes out the serial port. */
    tracer.SetLevel((int)level);
    con.Printf("loglevel %lu\n", level);
}

static void CmdUptime(const char* args, Stdlib::Printer& con)
{
    (void)args;
    auto time = GetBootTime();
    con.Printf("%lu.%lu\n", time.GetSecs(), time.GetUsecs());
}

static void CmdDate(const char* args, Stdlib::Printer& con)
{
    (void)args;

    static const ulong SecsPerMin  = 60;
    static const ulong MinsPerHour = 60;
    static const ulong HoursPerDay = 24;
    static const ulong DaysPerYear = 365;
    static const ulong DaysPerLeapYear = 366;
    static const ulong MonthsPerYear = 12;
    static const ulong FebruaryIndex = 2;
    static const ulong UnixEpochYear = 1970;

    ulong epoch = GetWallTimeSecs();
    if (epoch == 0)
    {
        con.Printf("wall clock not available\n");
        return;
    }

    /* Decompose Unix epoch into Y/M/D H:M:S */
    ulong secs = epoch;
    ulong s = secs % SecsPerMin; secs /= SecsPerMin;
    ulong m = secs % MinsPerHour; secs /= MinsPerHour;
    ulong h = secs % HoursPerDay; secs /= HoursPerDay;

    ulong days = secs; /* days since 1970-01-01 */
    ulong y = UnixEpochYear;
    while (true)
    {
        bool leap = (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0);
        ulong daysInYear = leap ? DaysPerLeapYear : DaysPerYear;
        if (days < daysInYear)
            break;
        days -= daysInYear;
        y++;
    }

    static const u16 daysInMonth[13] = {
        0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31
    };
    bool leap = (y % 4 == 0 && y % 100 != 0) || (y % 400 == 0);
    ulong mo = 1;
    while (mo <= MonthsPerYear)
    {
        ulong dim = daysInMonth[mo];
        if (mo == FebruaryIndex && leap)
            dim++;
        if (days < dim)
            break;
        days -= dim;
        mo++;
    }
    ulong d = days + 1;

    con.Printf("%lu-%lu-%lu %lu:%lu:%lu UTC\n", y, mo, d, h, m, s);
}

/* Percentages are per CPU, the way top has always reported them: a thread
   pinning one core reads 100%, and on a 20-CPU box the column can total
   2000%. Two samples a moment apart, because a cumulative runtime tells you
   what a task has ever done, not what it is doing. */
static void CmdTop(const char* args, Stdlib::Printer& con)
{
    static const ulong MaxSamples = 256;
    static const ulong DefaultIntervalMs = 500;
    static const ulong MaxIntervalMs = 10000;
    static const ulong Tag = 'Top ';

    const char* end;
    const char* tok = Stdlib::NextToken(args, end);

    ulong intervalMs = DefaultIntervalMs;
    if (tok)
    {
        char buf[16];
        Stdlib::TokenCopy(tok, end, buf, sizeof(buf));

        ulong ms;
        if (!Stdlib::ParseUlong(buf, ms) || ms == 0 || ms > MaxIntervalMs)
        {
            con.Printf("usage: top [interval-ms, 1-%lu]\n", MaxIntervalMs);
            return;
        }
        intervalMs = ms;
    }

    ulong bytes = MaxSamples * sizeof(TaskTable::CpuSample);
    auto* before = (TaskTable::CpuSample*)Mm::Alloc(bytes, Tag);
    if (!before)
    {
        con.Printf("top: out of memory\n");
        return;
    }

    auto* after = (TaskTable::CpuSample*)Mm::Alloc(bytes, Tag);
    if (!after)
    {
        Mm::Free(before);
        con.Printf("top: out of memory\n");
        return;
    }

    auto& table = TaskTable::GetInstance();

    /* Clock read before the first sample and after the second, so the window
       is never narrower than the runtime it is dividing: a task then reads a
       shade under its true share rather than a shade over 100%. */
    ulong t0 = GetBootTime().GetValue();
    size_t n0 = table.SampleCpu(before, MaxSamples);
    long deferred0 = GetPreemptDeferredCount();
    long deferredIdle0 = GetPreemptDeferredIdleCount();

    Sleep(intervalMs * Const::NanoSecsInMs);

    size_t n1 = table.SampleCpu(after, MaxSamples);
    long deferred1 = GetPreemptDeferredCount();
    long deferredIdle1 = GetPreemptDeferredIdleCount();
    ulong elapsed = GetBootTime().GetValue() - t0;

    if (elapsed == 0)
    {
        Mm::Free(after);
        Mm::Free(before);
        con.Printf("top: no time passed\n");
        return;
    }

    /* Tenths of a percent, so the column keeps one decimal without floats. */
    ulong permille[MaxSamples];
    for (size_t i = 0; i < n1; i++)
    {
        ulong was = 0;
        for (size_t j = 0; j < n0; j++)
        {
            if (before[j].Pid == after[i].Pid)
            {
                was = before[j].RuntimeNs;
                break;
            }
        }

        ulong delta = (after[i].RuntimeNs > was) ? (after[i].RuntimeNs - was) : 0;
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
    ulong mask = CpuTable::GetInstance().GetRunningCpus();
    for (ulong i = 0; i < MaxCpus; i++)
    {
        if (mask & (1UL << i))
            cpus++;
    }

    ulong busy = 0;
    for (size_t i = 0; i < n1; i++)
    {
        if (Stdlib::StrStr(after[i].Name, "idle") != after[i].Name)
            busy += permille[i];
    }

    con.Printf("cpus %lu, %lu tasks, %lu ms window, busy %lu.%lu%% of %lu00%%\n",
        cpus, (ulong)n1, intervalMs, busy / 10, busy % 10, cpus);
    con.Printf("  pid    cpu%%  name\n");

    for (size_t i = 0; i < n1; i++)
    {
        con.Printf("%lu %lu.%lu %s\n", after[i].Pid,
            permille[i] / 10, permille[i] % 10, after[i].Name);
    }

    Mm::Free(after);
    Mm::Free(before);

    con.Printf("task migrations since boot: %lu\n", (ulong)GetTaskMigrationCount());
    /* The window's count first: it is what the load being looked at did. The
       one since boot starts at one per CPU, on its idle task, that the boot
       self-test makes on purpose (TestPreemptDeferred). */
    con.Printf("preemptions deferred: %lu in the window, %lu of them on an idle task; "
        "%lu since boot, %lu on an idle task\n",
        (ulong)(deferred1 - deferred0), (ulong)(deferredIdle1 - deferredIdle0),
        (ulong)deferred1, (ulong)deferredIdle1);
}

/* A wait as the SchedStats bucket it fell in, an upper bound: "<4us" */
static void SchedBucketText(ulong bucket, char* buf, size_t size)
{
    if (bucket == ~0UL)
    {
        Stdlib::SnPrintf(buf, size, "-");
        return;
    }

    static const ulong UsInMs = 1000;
    static const ulong UsInSec = 1000 * UsInMs;

    /* The last bucket has no upper bound: everything from its floor up */
    bool last = (bucket + 1 >= SchedStats::Buckets);
    ulong us = last ? SchedStats::BucketLimitUs(bucket - 1) : SchedStats::BucketLimitUs(bucket);
    const char* rel = last ? ">=" : "<";

    if (us < UsInMs)
        Stdlib::SnPrintf(buf, size, "%s%luus", rel, us);
    else if (us < UsInSec)
        Stdlib::SnPrintf(buf, size, "%s%lu.%lums", rel, us / UsInMs, (us % UsInMs) / (UsInMs / 10));
    else
        Stdlib::SnPrintf(buf, size, "%s%lu.%lus", rel, us / UsInSec, (us % UsInSec) / (UsInSec / 10));
}

/* Nanoseconds, short: "410ns", "6.2us", "1.3ms", "2.1s" */
static void SchedNsText(ulong ns, char* buf, size_t size)
{
    if (ns < Const::NanoSecsInUsec)
        Stdlib::SnPrintf(buf, size, "%luns", ns);
    else if (ns < Const::NanoSecsInMs)
        Stdlib::SnPrintf(buf, size, "%lu.%luus", ns / Const::NanoSecsInUsec,
            (ns % Const::NanoSecsInUsec) / 100);
    else if (ns < Const::NanoSecsInSec)
        Stdlib::SnPrintf(buf, size, "%lu.%lums", ns / Const::NanoSecsInMs,
            (ns % Const::NanoSecsInMs) / (100 * Const::NanoSecsInUsec));
    else
        Stdlib::SnPrintf(buf, size, "%lu.%lus", ns / Const::NanoSecsInSec,
            (ns % Const::NanoSecsInSec) / (100 * Const::NanoSecsInMs));
}

/* The bucket that the given per mille of a histogram's entries are at or
   under; ~0UL for an empty one */
static ulong SchedPercentile(const ulong* hist, ulong permille)
{
    ulong count = 0;
    for (ulong i = 0; i < SchedStats::Buckets; i++)
        count += hist[i];
    if (count == 0)
        return ~0UL;

    ulong want = (count * permille + 999) / 1000;
    ulong seen = 0;
    for (ulong i = 0; i < SchedStats::Buckets; i++)
    {
        seen += hist[i];
        if (seen >= want)
            return i;
    }
    return SchedStats::Buckets - 1;
}

/* One line of `sched`: what a window's difference of two snapshots says,
   and the since-boot maxima of the later one */
static void SchedLine(const char* who, ulong tasks, const SchedStats::Snapshot& d,
    const SchedStats::Snapshot& now, Stdlib::Printer& con)
{
    ulong wakes = 0, delays = 0;
    for (ulong i = 0; i < SchedStats::Buckets; i++)
    {
        wakes += d.Wake[i];
        delays += d.Delay[i];
    }

    char w50[16], w99[16], w999[16], wmax[16], d50[16], d99[16], cavg[16], cmax[16];
    SchedBucketText(SchedPercentile(d.Wake, 500), w50, sizeof(w50));
    SchedBucketText(SchedPercentile(d.Wake, 990), w99, sizeof(w99));
    SchedBucketText(SchedPercentile(d.Wake, 999), w999, sizeof(w999));
    SchedNsText(now.WakeMaxNs, wmax, sizeof(wmax));
    SchedBucketText(SchedPercentile(d.Delay, 500), d50, sizeof(d50));
    SchedBucketText(SchedPercentile(d.Delay, 990), d99, sizeof(d99));

    ulong decisions = d.Decisions;
    SchedNsText((decisions != 0) ? CycleCounterDeltaToNs(d.DecisionCycles) / decisions : 0,
        cavg, sizeof(cavg));
    SchedNsText(CycleCounterDeltaToNs(now.DecisionMaxCycles), cmax, sizeof(cmax));

    /* Tenths, so averages keep a decimal without floats */
    ulong touch10 = (decisions != 0) ? (d.Touched * 10) / decisions : 0;
    ulong run10 = (d.Looks != 0) ? (d.Runnable * 10) / d.Looks : 0;
    ulong crowded = (d.Looks != 0) ? (d.Crowded * 100) / d.Looks : 0;

    con.Printf("%-4s %5lu %7lu %7s %7s %7s %7s %6lu %7s %7s %7lu %7s %7s %3lu.%lu %4lu %3lu.%lu %3lu %3lu%%\n",
        who, tasks, wakes, w50, w99, w999, wmax, delays, d50, d99,
        decisions, cavg, cmax, touch10 / 10, touch10 % 10, now.TouchedMax,
        run10 / 10, run10 % 10, now.RunnableMax, crowded);
}

static void SchedSum(SchedStats::Snapshot& into, const SchedStats::Snapshot& s)
{
    for (ulong i = 0; i < SchedStats::Buckets; i++)
    {
        into.Wake[i] += s.Wake[i];
        into.Delay[i] += s.Delay[i];
    }
    into.WakeMaxNs = Stdlib::Max(into.WakeMaxNs, s.WakeMaxNs);
    into.Decisions += s.Decisions;
    into.DecisionCycles += s.DecisionCycles;
    into.DecisionMaxCycles = Stdlib::Max(into.DecisionMaxCycles, s.DecisionMaxCycles);
    into.Touched += s.Touched;
    into.TouchedMax = Stdlib::Max(into.TouchedMax, s.TouchedMax);
    into.Looks += s.Looks;
    into.Runnable += s.Runnable;
    into.RunnableMax = Stdlib::Max(into.RunnableMax, s.RunnableMax);
    into.Crowded += s.Crowded;
}

/* d = b - a, the counters' part; the maxima are b's, since boot */
static void SchedDiff(SchedStats::Snapshot& d, const SchedStats::Snapshot& a,
    const SchedStats::Snapshot& b)
{
    for (ulong i = 0; i < SchedStats::Buckets; i++)
    {
        d.Wake[i] = b.Wake[i] - a.Wake[i];
        d.Delay[i] = b.Delay[i] - a.Delay[i];
    }
    d.WakeMaxNs = b.WakeMaxNs;
    d.Decisions = b.Decisions - a.Decisions;
    d.DecisionCycles = b.DecisionCycles - a.DecisionCycles;
    d.DecisionMaxCycles = b.DecisionMaxCycles;
    d.Touched = b.Touched - a.Touched;
    d.TouchedMax = b.TouchedMax;
    d.Looks = b.Looks - a.Looks;
    d.Runnable = b.Runnable - a.Runnable;
    d.RunnableMax = b.RunnableMax;
    d.Crowded = b.Crowded - a.Crowded;
}

/* `sched probe`: how long a woken task waits for a CPU that others keep
   busy -- the case a scheduler is chosen by. Spinners pinned to one CPU that
   never yield, so only the tick takes it from them; a waiter pinned beside
   them, asleep on an Event; and this command's task, on another CPU,
   signalling it a sample at a time and waiting to hear that it ran. Each
   wait is from the Signal() to the waiter running: the waker's IPI, and
   whatever the scheduler makes it wait behind. */
struct SchedProbe
{
    Event Ev;
    Atomic Stop;
    /* When the signal being answered was sent */
    Atomic SentAt;
    Atomic Answered;
    /* The waiter's own, read once it has exited: its exit and the join
       order them */
    ulong Hist[SchedStats::Buckets];
    ulong MaxNs;
    ulong Count;
    /* The lags the waiter was placed with as it woke, virtual ns */
    s64 LagMin;
    s64 LagMax;
    s64 LagSum;

    static const ulong Tag = 'SPrb';
};

static const ulong SchedProbeMaxSpinners = 16;
static const ulong SchedProbeMaxMs = 60000;
/* How long the waiter waits before it looks at Stop again */
static const ulong SchedProbeWaitNs = 100 * Const::NanoSecsInMs;
/* A signal not answered in this long is counted as such, and the probe
   moves on */
static const ulong SchedProbeAnswerNs = Const::NanoSecsInSec;
/* Between one answer and the next signal: the waiter is asleep again by
   then */
static const ulong SchedProbeGapNs = 2 * Const::NanoSecsInMs;

static void SchedProbeSpin(void* ctx)
{
    auto* p = static_cast<SchedProbe*>(ctx);
    while (p->Stop.Get() == 0)
        Pause();
}

static void SchedProbeWaiter(void* ctx)
{
    auto* p = static_cast<SchedProbe*>(ctx);
    while (p->Stop.Get() == 0)
    {
        if (!p->Ev.WaitFor(SchedProbeWaitNs))
            continue;

        ulong now = GetBootTime().GetValue();
        ulong sent = (ulong)p->SentAt.Get();
        if (sent != 0 && now >= sent)
        {
            ulong ns = now - sent;
            p->Hist[SchedStats::Bucket(ns)]++;
            p->MaxNs = Stdlib::Max(p->MaxNs, ns);
            p->Count++;

            /* The lag it was placed with as it woke */
            s64 lag = Task::GetCurrentTask()->Sched.PlacedLag;
            p->LagMin = Stdlib::Min(p->LagMin, lag);
            p->LagMax = Stdlib::Max(p->LagMax, lag);
            p->LagSum += lag;
        }
        p->SentAt.Set(0);
        p->Answered.Set(1);
    }
}

/* Runtime of task so far, under its lock as every UpdateRuntime() is made */
static ulong SchedProbeRuntime(Task* task)
{
    Stdlib::AutoLock lock(task->Lock);
    return task->Runtime.GetValue();
}

static void CmdSchedProbe(const char* args, Stdlib::Printer& con)
{
    auto& cpus = CpuTable::GetInstance();
    ulong mask = cpus.GetRunningCpus();
    ulong self = cpus.GetCurrentCpuId();

    ulong spinners = 2;
    ulong ms = 2000;
    /* The highest CPU that is not this one, unless told: this task signals
       from here */
    ulong cpu = ~0UL;
    for (ulong i = 0; i < MaxCpus; i++)
    {
        if ((mask & (1UL << i)) && i != self)
            cpu = i;
    }

    const char* end;
    for (const char* tok = Stdlib::NextToken(args, end); tok; tok = Stdlib::NextToken(end, end))
    {
        /* Zeroed: the prefixes are compared over their whole length, past
           the end of a token shorter than one */
        char buf[32] = {};
        Stdlib::TokenCopy(tok, end, buf, sizeof(buf));

        ulong value;
        bool ok = false;
        if (Stdlib::MemCmp(buf, "spinners=", 9) == 0)
        {
            ok = Stdlib::ParseUlong(buf + 9, value) && value <= SchedProbeMaxSpinners;
            if (ok)
                spinners = value;
        }
        else if (Stdlib::MemCmp(buf, "ms=", 3) == 0)
        {
            ok = Stdlib::ParseUlong(buf + 3, value) && value != 0 && value <= SchedProbeMaxMs;
            if (ok)
                ms = value;
        }
        else if (Stdlib::MemCmp(buf, "cpu=", 4) == 0)
        {
            ok = Stdlib::ParseUlong(buf + 4, value) && value < MaxCpus && (mask & (1UL << value));
            if (ok)
                cpu = value;
        }

        if (!ok)
        {
            con.Printf("usage: sched probe [spinners=0-%lu] [ms=1-%lu] [cpu=N]\n",
                SchedProbeMaxSpinners, SchedProbeMaxMs);
            return;
        }
    }

    if (cpu == ~0UL)
    {
        con.Printf("sched probe: needs a second cpu\n");
        return;
    }

    auto* p = Mm::TAlloc<SchedProbe, SchedProbe::Tag>();
    if (p == nullptr)
    {
        con.Printf("sched probe: out of memory\n");
        return;
    }
    Stdlib::MemSet(p->Hist, 0, sizeof(p->Hist));
    p->MaxNs = 0;
    p->LagMin = (s64)(~0UL >> 1);
    p->LagMax = -(s64)(~0UL >> 1);
    p->LagSum = 0;
    p->Count = 0;

    /* The waiter first, the spinners after it */
    Task* tasks[SchedProbeMaxSpinners + 1] = {};
    ulong started = 0;
    bool ok = true;
    for (ulong i = 0; ok && i <= spinners; i++)
    {
        char name[TaskNameLen];
        if (i == 0)
            Stdlib::SnPrintf(name, sizeof(name), "probe/wait");
        else
            Stdlib::SnPrintf(name, sizeof(name), "probe/spin%lu", i - 1);

        Task* task = Mm::TAlloc<Task, SchedProbe::Tag>(name);
        if (task == nullptr)
        {
            ok = false;
            break;
        }
        task->SetCpuAffinity(1UL << cpu);
        if (!task->Start((i == 0) ? SchedProbeWaiter : SchedProbeSpin, p))
        {
            task->Put();
            ok = false;
            break;
        }
        tasks[started++] = task;
    }

    ulong runAt[SchedProbeMaxSpinners + 1] = {};
    ulong sent = 0, unanswered = 0;
    ulong t0 = GetBootTime().GetValue();
    for (ulong i = 0; i < started; i++)
        runAt[i] = SchedProbeRuntime(tasks[i]);

    ulong until = t0 + ms * Const::NanoSecsInMs;
    while (ok && GetBootTime().GetValue() < until)
    {
        p->Answered.Set(0);
        p->SentAt.Set((long)GetBootTime().GetValue());
        p->Ev.Signal();
        sent++;

        ulong giveUp = GetBootTime().GetValue() + SchedProbeAnswerNs;
        while (p->Answered.Get() == 0 && GetBootTime().GetValue() < giveUp)
            Sleep(Const::NanoSecsInMs);
        if (p->Answered.Get() == 0)
            unanswered++;

        Sleep(SchedProbeGapNs);
    }
    ulong elapsed = GetBootTime().GetValue() - t0;

    ulong ran[SchedProbeMaxSpinners + 1] = {};
    for (ulong i = 0; i < started; i++)
        ran[i] = SchedProbeRuntime(tasks[i]) - runAt[i];

    p->Stop.Set(1);
    p->Ev.Signal();
    for (ulong i = 0; i < started; i++)
    {
        tasks[i]->Wait();
        tasks[i]->Put();
    }

    if (!ok)
    {
        con.Printf("sched probe: could not start its tasks\n");
    }
    else
    {
        char w50[16], w90[16], w99[16], wmax[16];
        SchedBucketText(SchedPercentile(p->Hist, 500), w50, sizeof(w50));
        SchedBucketText(SchedPercentile(p->Hist, 900), w90, sizeof(w90));
        SchedBucketText(SchedPercentile(p->Hist, 990), w99, sizeof(w99));
        SchedNsText(p->MaxNs, wmax, sizeof(wmax));

        con.Printf("sched probe: cpu %lu, %lu spinners, %lu ms: %lu signals, %lu answered, %lu not in %lu ms\n",
            cpu, spinners, ms, sent, p->Count, unanswered, SchedProbeAnswerNs / Const::NanoSecsInMs);
        con.Printf("  waited p50 %s p90 %s p99 %s max %s\n", w50, w90, w99, wmax);
        if (p->Count != 0)
            con.Printf("  woken with lag (virtual us): min %ld avg %ld max %ld\n",
                (long)(p->LagMin / (s64)Const::NanoSecsInUsec),
                (long)(p->LagSum / (s64)p->Count / (s64)Const::NanoSecsInUsec),
                (long)(p->LagMax / (s64)Const::NanoSecsInUsec));
        con.Printf("  ran, per mille of the window:");
        for (ulong i = 0; i < started && elapsed != 0; i++)
            con.Printf(" %s %lu", tasks[i]->GetName(), (ran[i] * 1000) / elapsed);
        con.Printf("\n");
    }

    p->~SchedProbe();
    Mm::Free(p);
}

/* `sched queue [cpu]`: what EEVDF has of each task on a CPU's queue that
   can run, the one on the CPU first -- its nice and slice, its lag (V less
   its virtual runtime: what it is owed), how far past V its deadline is,
   and whether it is eligible. Copied under the queue's lock, printed after. */
static const ulong SchedQueueMax = 64;

static void CmdSchedQueue(const char* args, Stdlib::Printer& con)
{
    auto& cpus = CpuTable::GetInstance();
    ulong mask = cpus.GetRunningCpus();
    ulong cpu = cpus.GetCurrentCpuId();

    const char* end;
    const char* tok = Stdlib::NextToken(args, end);
    if (tok)
    {
        char buf[16];
        Stdlib::TokenCopy(tok, end, buf, sizeof(buf));
        if (!Stdlib::ParseUlong(buf, cpu) || cpu >= MaxCpus || !(mask & (1UL << cpu)))
        {
            con.Printf("usage: sched queue [cpu]\n");
            return;
        }
    }

    auto* views = (TaskQueue::EntityView*)Mm::Alloc(SchedQueueMax * sizeof(TaskQueue::EntityView), SchedProbe::Tag);
    if (views == nullptr)
    {
        con.Printf("sched: out of memory\n");
        return;
    }

    ulong n = cpus.GetCpu(cpu).GetTaskQueue().View(views, SchedQueueMax);
    con.Printf("cpu %lu: %lu that can run, lag and deadline in virtual us from V\n", cpu, n);
    con.Printf("  %-6s %-5s %6s %9s %10s %10s %s\n", "pid", "nice", "slice", "lag", "deadline", "eligible", "name");
    for (ulong i = 0; i < n; i++)
    {
        auto& e = views[i];
        con.Printf("%s %-6lu %-5ld %6lu %9ld %10ld %10s %s\n", e.OnCpu ? "*" : " ", e.Pid, e.Nice,
            e.SliceNs / Const::NanoSecsInUsec, (long)(e.Lag / (s64)Const::NanoSecsInUsec),
            (long)(e.Deadline / (s64)Const::NanoSecsInUsec), e.Eligible ? "yes" : "no", e.Name);
    }

    Mm::Free(views);
}

/* `sched nice <pid> <nice>` and `sched slice <pid> <us>`: a task's share of
   its CPU, and how soon after it can it runs and for how long at a time */
static void CmdSchedSet(const char* args, Stdlib::Printer& con, bool nice)
{
    const char* end;
    const char* tok = Stdlib::NextToken(args, end);
    char buf[24] = {};
    ulong pid = 0;
    if (tok)
        Stdlib::TokenCopy(tok, end, buf, sizeof(buf));
    bool ok = tok && Stdlib::ParseUlong(buf, pid);

    tok = ok ? Stdlib::NextToken(end, end) : nullptr;
    long value = 0;
    if (tok)
    {
        char vbuf[24] = {};
        Stdlib::TokenCopy(tok, end, vbuf, sizeof(vbuf));
        bool negative = (vbuf[0] == '-');
        ulong magnitude = 0;
        ok = Stdlib::ParseUlong(negative ? vbuf + 1 : vbuf, magnitude) && magnitude < (1UL << 32);
        value = negative ? -(long)magnitude : (long)magnitude;
    }
    else
    {
        ok = false;
    }

    const long sliceMinUs = (long)(SchedEntity::MinSliceNs / Const::NanoSecsInUsec);
    const long sliceMaxUs = (long)(SchedEntity::MaxSliceNs / Const::NanoSecsInUsec);
    if (!ok || (nice && (value < SchedEntity::NiceMin || value > SchedEntity::NiceMax)) ||
        (!nice && (value < sliceMinUs || value > sliceMaxUs)))
    {
        con.Printf("usage: sched nice <pid> <%ld..%ld> | sched slice <pid> <%ld..%ld us>\n",
            SchedEntity::NiceMin, SchedEntity::NiceMax, sliceMinUs, sliceMaxUs);
        return;
    }

    ObjectPtr<Task> task(TaskTable::GetInstance().Lookup(pid));
    if (!task)
    {
        con.Printf("task %lu not found\n", pid);
        return;
    }

    if (nice)
    {
        SetTaskNice(task.Get(), value);
        con.Printf("%s (pid %lu): nice %ld\n", task->GetName(), pid, value);
    }
    else
    {
        SetTaskSlice(task.Get(), (ulong)value * Const::NanoSecsInUsec);
        con.Printf("%s (pid %lu): slice %ld us\n", task->GetName(), pid, value);
    }
}

/* What each CPU's scheduler did over a window (SchedStats): how long a woken
   task waited for the CPU -- at the median, the 99th and the 99.9th
   percentile, the buckets' bounds, and the longest since boot, exactly --
   how long one displaced while it could still run waited to run again, what
   a decision cost (the average, and the most since boot), how many tasks it
   touched, and how many tasks could run at the CPU's timer: on average, the
   most since boot, and how often two or more. */
static void CmdSched(const char* args, Stdlib::Printer& con)
{
    static const ulong DefaultIntervalMs = 1000;
    static const ulong MaxIntervalMs = 10000;
    static const ulong Tag = 'Schd';

    const char* end;
    const char* tok = Stdlib::NextToken(args, end);

    ulong intervalMs = DefaultIntervalMs;
    if (tok)
    {
        char buf[16];
        Stdlib::TokenCopy(tok, end, buf, sizeof(buf));

        if (Stdlib::StrCmp(buf, "probe") == 0)
        {
            CmdSchedProbe(end, con);
            return;
        }
        if (Stdlib::StrCmp(buf, "queue") == 0)
        {
            CmdSchedQueue(end, con);
            return;
        }
        if (Stdlib::StrCmp(buf, "nice") == 0 || Stdlib::StrCmp(buf, "slice") == 0)
        {
            CmdSchedSet(end, con, buf[0] == 'n');
            return;
        }

        ulong ms;
        if (!Stdlib::ParseUlong(buf, ms) || ms == 0 || ms > MaxIntervalMs)
        {
            con.Printf("usage: sched [interval-ms, 1-%lu] | sched probe [spinners=N] [ms=M] [cpu=N] | "
                "sched queue [cpu] | sched nice <pid> <n> | sched slice <pid> <us>\n",
                MaxIntervalMs);
            return;
        }
        intervalMs = ms;
    }

    /* Three of them a CPU: before, after, and the difference */
    ulong bytes = 3 * MaxCpus * sizeof(SchedStats::Snapshot);
    auto* snaps = (SchedStats::Snapshot*)Mm::Alloc(bytes, Tag);
    if (!snaps)
    {
        con.Printf("sched: out of memory\n");
        return;
    }
    SchedStats::Snapshot* before = &snaps[0];
    SchedStats::Snapshot* after = &snaps[MaxCpus];
    SchedStats::Snapshot* diff = &snaps[2 * MaxCpus];

    auto& cpus = CpuTable::GetInstance();
    ulong mask = cpus.GetRunningCpus();

    for (ulong i = 0; i < MaxCpus; i++)
    {
        if (mask & (1UL << i))
            cpus.GetCpu(i).GetTaskQueue().ReadStats(before[i]);
    }

    Sleep(intervalMs * Const::NanoSecsInMs);

    for (ulong i = 0; i < MaxCpus; i++)
    {
        if (mask & (1UL << i))
            cpus.GetCpu(i).GetTaskQueue().ReadStats(after[i]);
    }

    con.Printf("sched: %lu ms window; waits are bucket bounds, max since boot\n", intervalMs);
    con.Printf("%-4s %5s %7s %7s %7s %7s %7s %6s %7s %7s %7s %7s %7s %5s %4s %5s %3s %4s\n",
        "cpu", "tasks", "wakeups", "p50", "p99", "p99.9", "max", "delays", "p50", "p99",
        "decide", "avg", "max", "touch", "max", "run", "max", "2+");

    SchedStats::Snapshot total;
    Stdlib::MemSet(&total, 0, sizeof(total));
    SchedStats::Snapshot totalNow;
    Stdlib::MemSet(&totalNow, 0, sizeof(totalNow));
    ulong totalTasks = 0;

    for (ulong i = 0; i < MaxCpus; i++)
    {
        if (!(mask & (1UL << i)))
            continue;

        SchedDiff(diff[i], before[i], after[i]);
        ulong tasks = (ulong)cpus.GetCpu(i).GetTaskQueue().GetTaskCount();
        char who[8];
        Stdlib::SnPrintf(who, sizeof(who), "%lu", i);
        SchedLine(who, tasks, diff[i], after[i], con);

        SchedSum(total, diff[i]);
        SchedSum(totalNow, after[i]);
        totalTasks += tasks;
    }

    SchedLine("all", totalTasks, total, totalNow, con);

    Mm::Free(snaps);
}

/* Sampled on the per-CPU tick, so the resolution is the tick rate: enough
   to find where the time goes, not enough to see inside a short function. */
static void CmdProfile(const char* args, Stdlib::Printer& con)
{
    static const ulong DefaultMs = 1000;
    static const ulong MaxMs = 2000;

    const char* end;
    const char* tok = Stdlib::NextToken(args, end);

    ulong ms = DefaultMs;
    ulong pid = Profiler::NoPidFilter;
    ulong chains = Profiler::TopChains;

    /* Both consts: -Wformat reads a format through a pointer that cannot
       change */
    static const char* const Usage =
        "usage: profile [ms, 1-%lu] [pid|all] [chains]\n";

    if (tok)
    {
        char buf[24];
        Stdlib::TokenCopy(tok, end, buf, sizeof(buf));

        ulong value;
        if (!Stdlib::ParseUlong(buf, value) || value == 0 || value > MaxMs)
        {
            con.Printf(Usage, MaxMs);
            return;
        }
        ms = value;

        tok = Stdlib::NextToken(end, end);
        if (tok)
        {
            Stdlib::TokenCopy(tok, end, buf, sizeof(buf));

            /* `all` is how a chain count is given without a pid filter: the
               argument that matters on a console with no scrollback is the
               third one, and it should not require inventing a pid. */
            if (Stdlib::StrCmp(buf, "all") != 0)
            {
                if (!Stdlib::ParseUlong(buf, value))
                {
                    con.Printf(Usage, MaxMs);
                    return;
                }
                pid = value;
            }

            tok = Stdlib::NextToken(end, end);
            if (tok)
            {
                Stdlib::TokenCopy(tok, end, buf, sizeof(buf));
                if (!Stdlib::ParseUlong(buf, value) || value == 0 ||
                    value > Profiler::TopChains)
                {
                    con.Printf(Usage, MaxMs);
                    return;
                }
                chains = value;
            }
        }
    }

    auto& profiler = Profiler::GetInstance();
    if (!profiler.Start())
    {
        con.Printf("profile: could not start\n");
        return;
    }

    Sleep(ms * Const::NanoSecsInMs);
    profiler.Stop();

    con.Printf("profiled %lu ms", ms);
    if (pid != Profiler::NoPidFilter)
        con.Printf(", pid %lu only", pid);
    con.Printf("\n");

    profiler.Report(con, pid, chains);
}

static void CmdStacks(const char* args, Stdlib::Printer& con)
{
    (void)args;

    /* High-water marks, not a snapshot: each stack was filled with a pattern
       when it was created, and what is still intact is what it never
       reached. A spike that lasted a microsecond during boot shows up here
       just as clearly as a steady load. */
    ulong worstFree = (ulong)-1;

    con.Printf("kind id used size name\n");
    ReportCpuStacks(con, worstFree);
    TaskTable::GetInstance().Stacks(con, worstFree);

    if (worstFree == (ulong)-1)
    {
        con.Printf("no stack has been touched yet\n");
        return;
    }

    con.Printf("closest any stack came to its end: %lu bytes free\n", worstFree);
}


static void CmdPs(const char* args, Stdlib::Printer& con)
{
    (void)args;
    TaskTable::GetInstance().Ps(con);
}

static void CmdWatchdog(const char* args, Stdlib::Printer& con)
{
    (void)args;
    Watchdog::GetInstance().Dump(con);
}

static void CmdMemusage(const char* args, Stdlib::Printer& con)
{
    (void)args;
    auto& pt = Mm::PageTable::GetInstance();

    con.Printf("freePages: %lu\n", pt.GetFreePagesCount());
    con.Printf("totalPages: %lu\n", pt.GetTotalPagesCount());
}

static void CmdMemcheck(const char* args, Stdlib::Printer& con)
{
    (void)args;
    Mm::PageTable::GetInstance().CheckFreeList(con);
}

static void CmdMeminfo(const char* args, Stdlib::Printer& con)
{
    (void)args;
    auto& mmap = Mm::MemoryMap::GetInstance();
    auto& pt = Mm::PageTable::GetInstance();

    ulong usableRegions = 0;
    for (size_t i = 0; i < mmap.GetRegionCount(); i++)
    {
        ulong addr, len, type;
        if (!mmap.GetRegion(i, addr, len, type))
            break;

        con.Printf("0x%lX 0x%lX %s\n", addr, len,
            Mm::MemoryMap::GetRegionTypeName(type));

        if (type == Mm::MemoryMap::UsableRamType)
            usableRegions++;
    }

    /* The interesting number on a big machine is the third one: the kernel
       can only free-list RAM the bootstrap linear map reaches, and until
       that map grows, everything above it is memory the box has and the
       kernel does not know how to touch. */
    ulong mapLimit = Mm::BuiltinPageTable::GetInstance().GetMappedLimit();
    ulong usable = mmap.GetUsableRamBytes();
    ulong unused = mmap.GetUsableRamBytesAbove(mapLimit);

    con.Printf("usable: %lu MiB in %lu regions\n", usable / Const::MB, usableRegions);
    con.Printf("mapped: %lu MiB, bootstrap map ends at 0x%lX\n",
        (usable - unused) / Const::MB, mapLimit);
    if (unused != 0)
        con.Printf("unused: %lu MiB above the bootstrap map\n", unused / Const::MB);
    con.Printf("pages: %lu free of %lu\n",
        pt.GetFreePagesCount(), pt.GetTotalPagesCount());
}

static void CmdIrqstat(const char* args, Stdlib::Printer& con)
{
    (void)args;
    for (ulong i = 0; i < InterruptStats::Count; i++)
    {
        InterruptSource src = (InterruptSource)i;
        long count = InterruptStats::Get(src);
        if (count > 0)
            con.Printf("%s: %lu\n", InterruptStats::GetName(src), count);
    }
}

static void CmdPci(const char* args, Stdlib::Printer& con)
{
    (void)args;
    Pci::GetInstance().Dump(con);
}


/* crc32 <path>: the CRC-32 of a file, to check a copy against the host
   (python3 -c "import zlib,sys; print(hex(zlib.crc32(open(sys.argv[1],'rb').read())))" file) */
static void CmdCrc32(const char* args, Stdlib::Printer& con)
{
    static const ulong ChunkSize = 64 * 1024;

    const char* end;
    const char* pathStart = Stdlib::NextToken(args, end);
    if (pathStart == nullptr)
    {
        con.Printf("usage: crc32 <path>\n");
        return;
    }
    char path[MaxPath];
    Stdlib::TokenCopy(pathStart, end, path, sizeof(path));

    RustFile* file = FileOpen(path, FileRead);
    if (file == nullptr)
    {
        con.Printf("open failed\n");
        return;
    }

    u8* buf = (u8*)Mm::Alloc(ChunkSize, 0);
    if (buf == nullptr)
    {
        con.Printf("alloc failed\n");
        kernel_vfs_close(file);
        return;
    }

    u32 crc = 0;
    ulong total = 0;
    bool ok = true;
    for (;;)
    {
        ulong got = 0;
        if (kernel_vfs_read(file, buf, ChunkSize, &got) != 0)
        {
            ok = false;
            break;
        }
        if (got == 0)
            break;
        crc = Stdlib::Crc32Update(crc, buf, got);
        total += got;
    }

    Mm::Free(buf);
    kernel_vfs_close(file);

    if (ok)
        con.Printf("%s: crc32 0x%lX, %lu bytes\n", path, (ulong)crc, total);
    else
        con.Printf("read failed\n");
}

/* A GRUB environment block bigger than this is not one grub-editenv made */
static const ulong GrubenvMaxSize = 64 * 1024;

/* name=value with its terminator */
static const ulong GrubenvAssignmentSize =
    Stdlib::GrubEnvBlock::MaxNameLen + 1 + Stdlib::GrubEnvBlock::MaxValueLen + 1;

static bool GrubenvPrintVar(const char* name, const char* value, void* ctx)
{
    auto* con = static_cast<Stdlib::Printer*>(ctx);
    con->Printf("%s=%s\n", name, value);
    return true;
}

/* The whole file, or nullptr with the reason printed; size comes with it */
static char* GrubenvReadFile(const char* path, ulong& size, Stdlib::Printer& con)
{
    RustFile* file = FileOpen(path, FileRead);
    if (file == nullptr)
    {
        con.Printf("open failed\n");
        return nullptr;
    }

    size = kernel_vfs_size(file);
    if (size < Stdlib::GrubEnvBlock::MinSize || size > GrubenvMaxSize)
    {
        con.Printf("%s: %lu bytes is not a GRUB environment block\n", path, size);
        kernel_vfs_close(file);
        return nullptr;
    }

    char* block = (char*)Mm::Alloc(size, 0);
    if (block == nullptr)
    {
        con.Printf("alloc failed\n");
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
        con.Printf("read failed\n");
        Mm::Free(block);
        return nullptr;
    }
    return block;
}

/* grubenv <path>: the variables in a GRUB environment block, as
   grub-editenv list prints them. grubenv <path> name=value ...: set them
   (name= with nothing after it removes one) and write the block back where
   it was. This is how a running kernel arms a one-shot boot for the next
   GRUB -- see docs/real-hardware.md. */
static void CmdGrubenv(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* pathStart = Stdlib::NextToken(args, end);
    if (pathStart == nullptr)
    {
        con.Printf("usage: grubenv <path> [name=value ...]  (name= removes it)\n");
        return;
    }
    char path[MaxPath];
    Stdlib::TokenCopy(pathStart, end, path, sizeof(path));

    ulong size = 0;
    char* block = GrubenvReadFile(path, size, con);
    if (block == nullptr)
        return;

    Stdlib::GrubEnvBlock env(block, size);
    if (!env.IsValid())
    {
        con.Printf("%s: not a GRUB environment block\n", path);
        Mm::Free(block);
        return;
    }

    const char* tokStart = Stdlib::NextToken(end, end);
    if (tokStart == nullptr)
    {
        if (!env.ForEach(GrubenvPrintVar, &con))
            con.Printf("%s: malformed from here on\n", path);
        Mm::Free(block);
        return;
    }

    ulong changes = 0;
    while (tokStart != nullptr)
    {
        char assignment[GrubenvAssignmentSize];
        Stdlib::TokenCopy(tokStart, end, assignment, sizeof(assignment));

        const char* sep = Stdlib::StrChrOnce(assignment, '=');
        if (sep == nullptr || sep == assignment)
        {
            con.Printf("%s: expected name=value\n", assignment);
            Mm::Free(block);
            return;
        }
        assignment[sep - assignment] = '\0';
        const char* name = assignment;
        const char* value = sep + 1;

        if (*value == '\0')
        {
            if (env.Unset(name))
            {
                con.Printf("%s unset\n", name);
                changes++;
            }
            else
            {
                con.Printf("%s was not set\n", name);
            }
        }
        else if (env.Set(name, value))
        {
            con.Printf("%s=%s\n", name, value);
            changes++;
        }
        else
        {
            con.Printf("cannot set %s: not a name GRUB takes, or no room in %lu bytes\n",
                name, size);
            Mm::Free(block);
            return;
        }

        tokStart = Stdlib::NextToken(end, end);
    }

    if (changes == 0)
    {
        Mm::Free(block);
        return;
    }

    /* Back in place, at the same size: GRUB's save_env writes the file's own
       disk blocks, so the file has to keep them */
    RustFile* file = FileOpen(path, FileWrite);
    bool ok = (file != nullptr) && kernel_vfs_write(file, block, size) == 0;
    if (file != nullptr)
        kernel_vfs_close(file);
    Mm::Free(block);

    if (!ok)
    {
        con.Printf("%s: write failed\n", path);
        return;
    }
    if (kernel_vfs_sync() != 0)
        con.Printf("sync failed\n");
}

static void CmdVersion(const char* args, Stdlib::Printer& con)
{
    (void)args;
    con.Printf("nos %s (%s)\n", KERNEL_VERSION, KERNEL_GIT_REV);
}

static void DumpStackTrace(ulong* frames, size_t count, Stdlib::Printer& con)
{
    auto& symtab = SymbolTable::GetInstance();
    for (size_t i = 0; i < count; i++)
    {
        char where[SymbolTable::DescribeMax];
        if (symtab.DescribeReturn(frames[i], where, sizeof(where)))
            con.Printf("  [%lu] 0x%lX %s\n", (ulong)i, frames[i], where);
        else
            con.Printf("  [%lu] 0x%lX\n", (ulong)i, frames[i]);
    }
}

static void CmdBt(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* pidStart = Stdlib::NextToken(args, end);
    if (!pidStart)
    {
        con.Printf("usage: bt <pid>\n");
        return;
    }

    char pidBuf[16];
    Stdlib::TokenCopy(pidStart, end, pidBuf, sizeof(pidBuf));

    ulong pid = 0;
    if (!Stdlib::ParseUlong(pidBuf, pid))
    {
        con.Printf("invalid pid\n");
        return;
    }

    ObjectPtr<Task> task(TaskTable::GetInstance().Lookup(pid));
    if (!task)
    {
        con.Printf("task %lu not found\n", pid);
        return;
    }

    Task::Where where;
    task->Locate(where);

    char what[2 * SymbolTable::DescribeMax];
    where.Describe(what, sizeof(what));
    con.Printf("task %lu (%s) %s:\n", task->Pid, task->GetName(), what);
    DumpStackTrace(where.Frames, where.Count, con);
}

/* A stack that is no task's, for `panic offstack`: the panic path from
   where there is no task to ask about -- x86's double fault's stack, a
   parked CPU's -- with preemption on, as a running system has it. */
static char OffStack[4 * Const::PageSize] __attribute__((aligned(16)));

static void PanicOffStack(void* ctx)
{
    (void)ctx;
    Panic("user requested panic, off a task's stack");
}

static void CmdPanic(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* typeStart = Stdlib::NextToken(args, end);

    if (!typeStart)
    {
        Panic("user requested panic");
    }

    char type[16];
    Stdlib::TokenCopy(typeStart, end, type, sizeof(type));

    if (Stdlib::StrCmp(type, "pf") == 0)
    {
        con.Printf("triggering page fault...\n");
        volatile int* p = nullptr;
        // cppcheck-suppress nullPointer
        *p = 0;
    }
    else if (Stdlib::StrCmp(type, "div0") == 0)
    {
        con.Printf("triggering divide by zero...\n");
        volatile int zero = 0;
        // cppcheck-suppress zerodiv
        volatile int x = 1 / zero;
        (void)x;
    }
    else if (Stdlib::StrCmp(type, "ud") == 0)
    {
        con.Printf("triggering invalid opcode...\n");
        Hal::UndefInstr();
    }
    else if (Stdlib::StrCmp(type, "df") == 0)
    {
        con.Printf("triggering double fault...\n");
        if (!Hal::DoubleFault())
            con.Printf("no double fault here: an exception has no stack of its own on this architecture\n");
    }
    else if (Stdlib::StrCmp(type, "offstack") == 0)
    {
        con.Printf("panicking on a stack that is no task's...\n");
        InterruptDisable();
        Hal::RunOnStack((ulong)&OffStack[sizeof(OffStack)], PanicOffStack, nullptr);
    }
    else
    {
        con.Printf("usage: panic [pf|div0|ud|df|offstack]\n");
        con.Printf("  (no arg) - direct panic\n");
        con.Printf("  pf       - page fault (null deref)\n");
        con.Printf("  div0     - divide by zero\n");
        con.Printf("  ud       - invalid opcode\n");
        con.Printf("  df       - double fault, on its own stack (x86-64)\n");
        con.Printf("  offstack - panic on a stack that is no task's\n");
    }
}

/* Each in a task of its own, which the shell waits for only so long: an
   unload waits out a module command still running, however long it runs */
static void CmdInsmod(const char* args, Stdlib::Printer& con)
{
    if (args[0] == '\0')
    {
        con.Printf("usage: insmod <path> [parameters]\n");
        return;
    }

    ModuleTable::GetInstance().StartLoad(args, con);
}

static void CmdRmmod(const char* args, Stdlib::Printer& con)
{
    if (args[0] == '\0')
    {
        con.Printf("usage: rmmod <name>\n");
        return;
    }

    ModuleTable::GetInstance().StartUnload(args, con);
}

static void CmdLsmod(const char* args, Stdlib::Printer& con)
{
    (void)args;
    ModuleTable::GetInstance().Dump(con);
}

/* /etc/rc: shell commands run once at boot, after the network is set up --
   what loads and starts a module like sshd on a machine nobody can reach
   until it has. `rc` shows and edits it; rc=off on the kernel command line
   skips it at boot. */
static const char RcPath[] = "/etc/rc";
static const char RcDir[] = "/etc";
/* The most of a script read, and the longest line run: long enough for a
   guest's `hv start` with a distribution's kernel command line, which runs
   past two hundred characters before it says anything of its own. Lines
   are copied through a buffer this long on the stack of the task that runs
   the script, a kernel task's 64 KiB. */
static const ulong ScriptSizeMax = 16 * Const::KB;
static const ulong ScriptLineMax = 1023;
static const ulong ScriptTag = 'Rc  ';

/* A script, NUL-terminated, in a buffer from Mm::Alloc the caller frees --
   or nullptr, said on out, when it cannot be read. Read through the
   whole-file calls, which look for it where a cut-short write may have left
   it as well as where it belongs. Past ScriptSizeMax only its whole lines
   are taken, whole comes back false, and it is said: a command cut at the
   limit would run as some other command. */
static char* ReadScript(const char* path, ulong& size, bool& whole, Stdlib::Printer& out)
{
    const ulong pathLen = Stdlib::StrLen(path);
    const long total = kernel_file_size(path, pathLen);
    if (total < 0)
    {
        out.Printf("rc: cannot open %s\n", path);
        return nullptr;
    }

    char* text = static_cast<char*>(Mm::Alloc(ScriptSizeMax + 1, ScriptTag));
    if (text == nullptr)
    {
        out.Printf("rc: no memory to read %s\n", path);
        return nullptr;
    }

    const long got = kernel_file_read(path, pathLen, text, ScriptSizeMax);
    if (got < 0)
    {
        Mm::Free(text);
        out.Printf("rc: cannot read %s\n", path);
        return nullptr;
    }
    size = (ulong)got;
    whole = ((ulong)total <= size);
    if (!whole)
    {
        ulong keep = size;
        while (keep > 0 && text[keep - 1] != '\n')
            keep--;
        out.Printf("rc: %s is %lu bytes; only its whole lines in the first %lu are read\n",
            path, total, size);
        size = keep;
    }

    text[size] = '\0';
    return text;
}

/* The line of text at p, blanks and a CR trimmed off either end, with p
   moved past it; false at the end of the text */
static bool NextScriptLine(const char*& p, const char*& line, ulong& len)
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
class LogPrinter final : public Stdlib::Printer
{
public:
    LogPrinter(const char* prefix, Stdlib::Printer* echo, char* buf, ulong size)
        : Prefix(prefix)
        , Echo(echo)
        , Buf(buf)
        , Size(size)
        , Len(0)
        , Dropped(0)
    {
    }

    virtual PRINTF_FORMAT(2, 3) void Printf(const char *fmt, ...) override
    {
        va_list args;
        va_start(args, fmt);
        VPrintf(fmt, args);
        va_end(args);
    }

    virtual PRINTF_FORMAT(2, 0) void VPrintf(const char *fmt, va_list args) override
    {
        char text[FormatMax];
        if (Stdlib::VsnPrintf(text, sizeof(text), fmt, args) < 0)
            return;
        Add(text);
    }

    virtual void PrintString(const char *s) override
    {
        if (s != nullptr)
            Add(s);
    }

    virtual void Backspace() override
    {
    }

    /* A command is done: what it printed goes to the log, a line at a time,
       a line longer than the log takes in pieces */
    void Commit()
    {
        ulong start = 0;
        while (start < Len)
        {
            ulong end = start;
            while (end < Len && Buf[end] != '\n' && end - start < LineMax)
                end++;

            char line[LineMax + 1];
            ulong n = 0;
            for (ulong i = start; i < end; i++)
            {
                if (Buf[i] != '\r')
                    line[n++] = Buf[i];
            }
            line[n] = '\0';
            if (n != 0)
                Trace(0, "%s%s", Prefix, line);

            start = (end < Len && Buf[end] == '\n') ? end + 1 : end;
        }
        if (Dropped != 0)
            Trace(0, "%s[%lu more bytes it printed not logged]", Prefix, Dropped);

        Len = 0;
        Dropped = 0;
    }

private:
    LogPrinter(const LogPrinter& other) = delete;
    LogPrinter& operator=(const LogPrinter& other) = delete;

    void Add(const char* s)
    {
        if (Echo != nullptr)
            Echo->PrintString(s);

        for (; *s != '\0'; s++)
        {
            if (Len == Size)
                Dropped++;
            else
                Buf[Len++] = *s;
        }
    }

    /* Room, in the trace line's 256 bytes, for its own prefix */
    static const ulong LineMax = 160;
    static const ulong FormatMax = 512;

    const char* Prefix;
    Stdlib::Printer* Echo;
    char* Buf;
    ulong Size;
    ulong Len;
    ulong Dropped;
};

/* What a boot script command printed, into the log: RunScript's step */
static void CommitLog(void* ctx)
{
    static_cast<LogPrinter*>(ctx)->Commit();
}

/* What of a boot script command's output the log takes */
static const ulong RcLogSize = 8 * Const::KB;

static void RcShow(Stdlib::Printer& con)
{
    if (kernel_file_size(RcPath, Stdlib::StrLen(RcPath)) < 0)
    {
        con.Printf("rc: no %s -- rc add <command line> makes one\n", RcPath);
        return;
    }

    ulong size = 0;
    bool whole = true;
    char* text = ReadScript(RcPath, size, whole, con);
    if (text == nullptr)
        return;

    const char* p = text;
    const char* line = nullptr;
    ulong len = 0;
    ulong number = 0;
    char shown[ScriptLineMax + 1];
    while (NextScriptLine(p, line, len))
    {
        number++;
        ulong n = (len < ScriptLineMax) ? len : ScriptLineMax;
        Stdlib::MemCpy(shown, line, n);
        shown[n] = '\0';
        con.Printf("%lu  %s\n", number, shown);
    }
    if (number == 0)
        con.Printf("rc: %s is empty\n", RcPath);

    Mm::Free(text);
}

/* Writes a new /etc/rc: the lines of the old one but the one numbered skip
   (0: none), then add if there is one -- through kernel_file_write, since
   what it is for is the next boot, and a full disk must not leave it empty.
   One edit at a time (RcLock): two at once would each write over the other
   one's line. */
static bool RcRewrite(ulong skip, const char* add, Stdlib::Printer& con)
{
    Stdlib::AutoLock lock(Cmd::GetInstance().GetRcLock());

    ulong size = 0;
    char* old = nullptr;
    if (kernel_file_size(RcPath, Stdlib::StrLen(RcPath)) >= 0)
    {
        bool whole = true;
        old = ReadScript(RcPath, size, whole, con);
        if (old == nullptr)
            return false;
        if (!whole)
        {
            Mm::Free(old);
            con.Printf("rc: %s is too large to edit with rc\n", RcPath);
            return false;
        }
    }
    else if (skip != 0)
    {
        con.Printf("rc: no %s\n", RcPath);
        return false;
    }
    else if (kernel_dir_create(RcDir, Stdlib::StrLen(RcDir)) != 0)
    {
        con.Printf("rc: cannot make %s\n", RcDir);
        return false;
    }

    if (skip != 0)
    {
        const char* p = old;
        const char* line = nullptr;
        ulong len = 0;
        ulong lines = 0;
        while (NextScriptLine(p, line, len))
            lines++;
        if (skip > lines)
        {
            Mm::Free(old);
            con.Printf("rc: %s has no line %lu\n", RcPath, skip);
            return false;
        }
    }

    ulong addLen = (add != nullptr) ? Stdlib::StrLen(add) : 0;
    if (size + addLen + 1 > ScriptSizeMax)
    {
        if (old != nullptr)
            Mm::Free(old);
        con.Printf("rc: %s would pass %lu bytes\n", RcPath, ScriptSizeMax);
        return false;
    }

    char* text = static_cast<char*>(Mm::Alloc(ScriptSizeMax + 1, ScriptTag));
    if (text == nullptr)
    {
        if (old != nullptr)
            Mm::Free(old);
        con.Printf("rc: no memory\n");
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
        while (NextScriptLine(p, line, len))
        {
            number++;
            if (number == skip)
                continue;
            Stdlib::MemCpy(text + pos, line, len);
            pos += len;
            text[pos++] = '\n';
        }
        Mm::Free(old);
    }
    if (addLen != 0)
    {
        Stdlib::MemCpy(text + pos, add, addLen);
        pos += addLen;
        text[pos++] = '\n';
    }

    bool ok = kernel_file_write(RcPath, Stdlib::StrLen(RcPath), text, pos) == 0;
    Mm::Free(text);
    if (!ok)
    {
        con.Printf("rc: cannot write %s\n", RcPath);
        return false;
    }
    return true;
}

static void CmdRc(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* word = Stdlib::NextToken(args, end);
    if (word == nullptr)
    {
        RcShow(con);
        return;
    }

    char verb[8];
    Stdlib::TokenCopy(word, end, verb, sizeof(verb));

    const char* rest = end;
    while (*rest == ' ')
        rest++;

    if (Stdlib::StrCmp(verb, "add") == 0)
    {
        ulong len = Stdlib::StrLen(rest);
        if (len == 0)
            con.Printf("usage: rc add <command line>\n");
        else if (len > ScriptLineMax)
            con.Printf("rc: at most %lu characters a line\n", ScriptLineMax);
        else if (RcRewrite(0, rest, con))
            con.Printf("rc: added to %s: %s\n", RcPath, rest);
    }
    else if (Stdlib::StrCmp(verb, "del") == 0)
    {
        ulong number = 0;
        if (!Stdlib::ParseUlong(rest, number) || number == 0)
            con.Printf("usage: rc del <n> -- n as rc numbers the lines\n");
        else if (RcRewrite(number, nullptr, con))
            RcShow(con);
    }
    else if (Stdlib::StrCmp(verb, "clear") == 0)
    {
        Stdlib::AutoLock lock(Cmd::GetInstance().GetRcLock());
        const ulong pathLen = Stdlib::StrLen(RcPath);
        if (kernel_file_size(RcPath, pathLen) < 0)
        {
            con.Printf("rc: no %s\n", RcPath);
        }
        else if (kernel_file_remove(RcPath, pathLen) == 0)
        {
            /* Both of the pair go, should a cut-short edit have left both */
            con.Printf("rc: removed %s\n", RcPath);
        }
        else
        {
            con.Printf("rc: cannot remove %s\n", RcPath);
        }
    }
    else if (Stdlib::StrCmp(verb, "run") == 0)
    {
        Cmd::GetInstance().RunScript(RcPath, con);
    }
    else
    {
        con.Printf("usage: rc [add <command line> | del <n> | clear | run]\n");
    }
}

// Forward declaration - CmdHelp needs the Commands array defined below
static void CmdHelp(const char* args, Stdlib::Printer& con);

static const CmdEntry Commands[] = {
    { "cls",       CmdCls,       "cls - clear screen" },
    { "cpu",       CmdCpu,       "cpu - dump cpu state" },
    { "lscpu",     CmdLscpu,     "lscpu - identify the cpu and the features the kernel needs" },
    { "dmesg",     CmdDmesg,     "dmesg [lines] [filter] - dump kernel log" },
    { "loglevel",  CmdLoglevel,  "loglevel [N] - show or set trace level" },
    { "uptime",    CmdUptime,    "uptime - show uptime" },
    { "date",      CmdDate,      "date - show wall clock time" },
    { "ps",        CmdPs,        "ps - show tasks" },
    { "stacks",    CmdStacks,    "stacks - stack high-water marks" },
    { "top",       CmdTop,       "top [ms] - per-task cpu use over a sampling window" },
    { "sched",     CmdSched,     "sched [ms] | sched probe [spinners=N] [ms=M] [cpu=N] | sched queue [cpu] | sched nice <pid> <n> | sched slice <pid> <us> - what each cpu's scheduler did over a window; how long a woken task waits behind spinners; what EEVDF has of a cpu's tasks; a task's weight and slice" },
    { "profile",   CmdProfile,   "profile [ms] [pid] - sample where the kernel spends its time" },
    { "watchdog",  CmdWatchdog,  "watchdog - show watchdog stats" },
    { "memusage",  CmdMemusage,  "memusage - show memory usage stats" },
    { "meminfo",   CmdMeminfo,   "meminfo - show the firmware memory map and what of it is used" },
    { "memcheck",  CmdMemcheck,  "memcheck - verify no reserved page reached the free list" },
    { "irqstat",   CmdIrqstat,   "irqstat - show interrupt statistics" },
    { "pci",       CmdPci,       "pci - show pci devices" },
    { "crc32",     CmdCrc32,     "crc32 <path> - CRC-32 of a file" },
    { "grubenv",   CmdGrubenv,   "grubenv <path> [name=value ...] - show or set GRUB environment variables" },
    { "insmod",    CmdInsmod,    "insmod <path> [params] - load a kernel module (.ko), handing its init the parameters" },
    { "rmmod",     CmdRmmod,     "rmmod <name> - unload a kernel module" },
    { "lsmod",     CmdLsmod,     "lsmod - list the loaded kernel modules" },
    { "rc",        CmdRc,        "rc [add <command line>|del <n>|clear|run] - show or edit /etc/rc, run at boot" },
    { "version",   CmdVersion,   "version - show kernel version" },
    { "bt",        CmdBt,        "bt <pid> - show task backtrace" },
    { "panic",     CmdPanic,     "panic [pf|div0|ud|df|offstack] - trigger kernel panic" },
    { "poweroff",  CmdPoweroff,  "poweroff - power off (ACPI S5)" },
    { "shutdown",  CmdPoweroff,  nullptr },
    { "reboot",    CmdReboot,    "reboot - reset system" },
    { "help",      CmdHelp,      "help - help" },
    { nullptr,     nullptr,      nullptr },
};

static void CmdHelp(const char* args, Stdlib::Printer& con)
{
    (void)args;
    for (ulong i = 0; Commands[i].Name != nullptr; i++)
    {
        if (Commands[i].Help != nullptr)
            con.Printf("%s\n", Commands[i].Help);
    }

    Cmd::GetInstance().DynamicHelp(con);
}

Cmd::Cmd()
    : TaskPtr(nullptr)
    , Shutdown(false)
    , Reboot(false)
    , Active(false)
    , ScriptRunning(false)
    , DynamicGeneration(0)
{
    Stdlib::MemSet(Dynamic, 0, sizeof(Dynamic));
}

Cmd::~Cmd()
{
    if (TaskPtr != nullptr)
    {
        TaskPtr->Put();
        TaskPtr = nullptr;
    }
}

void Cmd::Dispatch(const char *cmd, Stdlib::Printer& out)
{
    bool found = false;
    for (ulong i = 0; Commands[i].Name != nullptr; i++)
    {
        ulong nameLen = Stdlib::StrLen(Commands[i].Name);
        if (Stdlib::StrCmp(cmd, Commands[i].Name) == 0)
        {
            Commands[i].Handler("", out);
            found = true;
            break;
        }
        else if (Stdlib::MemCmp(cmd, Commands[i].Name, nameLen) == 0 && cmd[nameLen] == ' ')
        {
            Commands[i].Handler(cmd + nameLen + 1, out);
            found = true;
            break;
        }
    }

    if (!found)
        found = GetInstance().DispatchDynamic(cmd, out);

    if (!found)
        out.Printf("command '%s' not found\n", cmd);
}

bool Cmd::DispatchDynamic(const char* cmd, Stdlib::Printer& out)
{
    DynamicHandler handler = nullptr;
    void* ctx = nullptr;
    const char* args = "";
    ulong slot = 0;

    {
        Stdlib::AutoLock lock(DynamicLock);
        for (ulong i = 0; i < DynamicMax; i++)
        {
            DynamicCmd& entry = Dynamic[i];
            if (entry.Handle == 0 || entry.Removing)
                continue;

            ulong nameLen = Stdlib::StrLen(entry.Name);
            if (Stdlib::StrCmp(cmd, entry.Name) == 0)
                args = "";
            else if (Stdlib::MemCmp(cmd, entry.Name, nameLen) == 0 && cmd[nameLen] == ' ')
                args = cmd + nameLen + 1;
            else
                continue;

            entry.Running++;
            handler = entry.Handler;
            ctx = entry.Ctx;
            slot = i;
            break;
        }
    }

    if (handler == nullptr)
        return false;

    /* Without the lock: the handler is the module's code and may take as
       long as it likes. Running keeps UnregisterDynamic -- and with it the
       module's unload -- waiting until it returns. */
    handler(ctx, args, Stdlib::StrLen(args), &out);

    Stdlib::AutoLock lock(DynamicLock);
    Dynamic[slot].Running--;
    return true;
}

ulong Cmd::RegisterDynamic(const char* name, ulong nameLen, const char* help, ulong helpLen,
    DynamicHandler handler, void* ctx)
{
    if (name == nullptr || handler == nullptr || nameLen == 0 || nameLen > DynamicNameMax)
        return 0;

    char key[DynamicNameMax + 1];
    for (ulong i = 0; i < nameLen; i++)
    {
        if (name[i] <= ' ' || name[i] > '~')
            return 0;
        key[i] = name[i];
    }
    key[nameLen] = '\0';

    for (ulong i = 0; Commands[i].Name != nullptr; i++)
    {
        if (Stdlib::StrCmp(Commands[i].Name, key) == 0)
            return 0;
    }

    Stdlib::AutoLock lock(DynamicLock);

    DynamicCmd* entry = nullptr;
    ulong slot = 0;
    for (ulong i = 0; i < DynamicMax; i++)
    {
        if (Dynamic[i].Handle == 0)
        {
            if (entry == nullptr)
            {
                entry = &Dynamic[i];
                slot = i;
            }
        }
        else if (Stdlib::StrCmp(Dynamic[i].Name, key) == 0)
        {
            return 0;
        }
    }

    if (entry == nullptr)
        return 0;

    ulong helpCopy = (help != nullptr) ? helpLen : 0;
    if (helpCopy > DynamicHelpMax)
        helpCopy = DynamicHelpMax;
    for (ulong i = 0; i < helpCopy; i++)
        entry->Help[i] = (help[i] >= ' ' && help[i] <= '~') ? help[i] : '?';
    entry->Help[helpCopy] = '\0';

    Stdlib::MemCpy(entry->Name, key, nameLen + 1);
    entry->Handler = handler;
    entry->Ctx = ctx;
    entry->Running = 0;
    entry->Removing = false;
    DynamicGeneration++;
    entry->Handle = (DynamicGeneration << DynamicSlotBits) | (slot + 1);
    return entry->Handle;
}

void Cmd::UnregisterDynamic(ulong handle)
{
    const ulong index = handle & ((1UL << DynamicSlotBits) - 1);
    if (index == 0 || index > DynamicMax)
        return;

    DynamicCmd& entry = Dynamic[index - 1];
    {
        Stdlib::AutoLock lock(DynamicLock);
        if (entry.Handle != handle)
            return;
        entry.Removing = true;
    }

    /* Wait out the calls already made: the module may free the handler's ctx,
       and unload its code, the moment this returns */
    for (;;)
    {
        {
            Stdlib::AutoLock lock(DynamicLock);

            /* A second unregister of the same handle may have finished first
               and the slot found a new user since: not ours to clear */
            if (entry.Handle != handle)
                return;

            if (entry.Running == 0)
            {
                entry.Handle = 0;
                return;
            }
        }
        Sleep(DynamicPollNs);
    }
}

void Cmd::DynamicHelp(Stdlib::Printer& out)
{
    for (ulong i = 0; i < DynamicMax; i++)
    {
        char help[DynamicHelpMax + 1];
        help[0] = '\0';
        {
            Stdlib::AutoLock lock(DynamicLock);
            const DynamicCmd& entry = Dynamic[i];
            if (entry.Handle != 0 && !entry.Removing)
            {
                const char* text = (entry.Help[0] != '\0') ? entry.Help : entry.Name;
                ulong len = Stdlib::StrLen(text);
                if (len > DynamicHelpMax)
                    len = DynamicHelpMax;
                Stdlib::MemCpy(help, text, len);
                help[len] = '\0';
            }
        }

        /* Printed without the lock: a console line can take a while */
        if (help[0] != '\0')
            out.Printf("%s\n", help);
    }
}

bool Cmd::RunScript(const char* path, Stdlib::Printer& out, ScriptStep step, void* stepCtx)
{
    bool running;
    {
        Stdlib::AutoLock lock(Lock);
        running = ScriptRunning;
        ScriptRunning = true;
    }
    /* Said out of the lock, which keeps interrupts off: out may be an SSH
       session's, and what is printed there may wait for the network */
    if (running)
    {
        out.Printf("rc: a script is running already -- one that ran itself would never end\n");
        return false;
    }

    ulong size = 0;
    bool whole = true;
    char* text = ReadScript(path, size, whole, out);
    bool ok = (text != nullptr);
    if (ok)
    {
        const char* p = text;
        const char* line = nullptr;
        ulong len = 0;
        ulong number = 0;
        char cmd[ScriptLineMax + 1];
        while (NextScriptLine(p, line, len))
        {
            number++;
            if (len == 0 || line[0] == '#')
                continue;
            if (len > ScriptLineMax)
            {
                out.Printf("rc: line %lu is longer than %lu characters, skipped\n", number, ScriptLineMax);
                continue;
            }
            Stdlib::MemCpy(cmd, line, len);
            cmd[len] = '\0';
            out.Printf("> %s\n", cmd);
            Dispatch(cmd, out);
            if (step != nullptr)
                step(stepCtx);
        }
        Mm::Free(text);
    }

    {
        Stdlib::AutoLock lock(Lock);
        ScriptRunning = false;
    }
    return ok;
}

void Cmd::RunBootScript()
{
    if (kernel_file_size(RcPath, Stdlib::StrLen(RcPath)) < 0)
        return;

    if (Parameters::GetInstance().IsRcOff())
    {
        Trace(0, "rc: %s skipped, rc=off", RcPath);
        return;
    }

    /* With no room to keep it, what the commands print is on the screen
       alone, and the log says how much */
    char* buf = static_cast<char*>(Mm::Alloc(RcLogSize, ScriptTag));

    Trace(0, "rc: running %s", RcPath);
    LogPrinter log("rc: ", &Console::GetInstance(), buf, (buf != nullptr) ? RcLogSize : 0);
    RunScript(RcPath, log, CommitLog, &log);
    log.Commit();
    Trace(0, "rc: %s done", RcPath);

    if (buf != nullptr)
        Mm::Free(buf);
}

/* The flags below are polled by the BSP's idle task, between halts. The
   scheduler runs an idle task only when nothing else on its queue can run
   -- and on a CPU carrying the shell, DHCP and USB poll tasks that could be
   never (a task that slept used to yield rather than block, and one that
   polls still does): the request would sit unseen for good. Let that task
   take its turn as an ordinary one from here on; it has nothing left to do
   but notice. */
static void WakeShutdownWatch()
{
    auto& cpus = CpuTable::GetInstance();
    Task* idle = cpus.GetCpu(cpus.GetBspIndex()).GetIdleTask();
    if (idle != nullptr)
        idle->ClearIdle();
}

void Cmd::RequestShutdown()
{
    Shutdown = true;
    WakeShutdownWatch();
}

void Cmd::RequestReboot()
{
    Reboot = true;
    WakeShutdownWatch();
}

bool Cmd::ShouldShutdown()
{
    return Shutdown;
}

bool Cmd::ShouldReboot()
{
    return Reboot;
}

void Cmd::Stop()
{
    if (Active)
    {
        BugOn(TaskPtr == nullptr);
        Active = false;
        TaskPtr->SetStopping();
        TaskPtr->Wait();
    }
}

void Cmd::StopDhcp()
{
    rust_net_dhcp_stop();
}

bool Cmd::Start()
{
    if (TaskPtr != nullptr)
        return false;

    auto task = Mm::TAlloc<Task, Tag>("cmd");
    if (task == nullptr)
        return false;

    {
        Stdlib::AutoLock lock(Lock);
        if (TaskPtr == nullptr)
        {
            TaskPtr = task;
        }
    }

    if (TaskPtr != task)
    {
        task->Put();
        return false;
    }

    if (!TaskPtr->Start(&Cmd::RunFunc, this))
    {
        {
            Stdlib::AutoLock lock(Lock);
            task = TaskPtr;
            TaskPtr = nullptr;
        }
        task->Put();
        return false;
    }

    {
        Stdlib::AutoLock lock(Lock);
        Active = true;
    }
    return true;
}

void Cmd::ShowBanner(Stdlib::Printer& out)
{
    out.Printf("\n");
    out.Printf("  _   _  ___  ____\n");
    out.Printf(" | \\ | |/ _ \\/ ___|\n");
    out.Printf(" |  \\| | | | \\___ \\\n");
    out.Printf(" | |\\  | |_| |___) |\n");
    out.Printf(" |_| \\_|\\___/|____/\n");
    out.Printf("\n");
}

void Cmd::Run()
{
    auto& con = Console::GetInstance();

    /* Wait for startup trace output to settle before suppressing console */
    Sleep(100 * Const::NanoSecsInMs);

    Tracer::GetInstance().SetConsoleSuppressed(true);

    ShowBanner(con);

    if (Parameters::GetInstance().IsDhcpAuto())
        rust_net_dhcp_auto(&con);

    RunBootScript();

    Editor.Prompt(con);

    while (!Task::GetCurrentTask()->IsStopping())
    {
        char c = 0;
        bool hasKey = false;
        {
            Stdlib::AutoLock lock(Lock);
            if (!Buf.IsEmpty())
            {
                c = Buf.Get();
                hasKey = true;
            }
        }

        /* Everything typed is taken before the task sleeps again: an arrow
           is three bytes, and a line may be pasted */
        if (!hasKey)
        {
            Sleep(10 * Const::NanoSecsInMs);
            continue;
        }

        if (Editor.Key(c, con))
        {
            /* A line of nothing but spaces is a new prompt, as over SSH */
            const char* line = Editor.GetLine();
            const char* word = line;
            while (*word == ' ')
                word++;
            if (*word != '\0')
                Dispatch(line, con);
            Editor.Prompt(con);
        }
    }

    Tracer::GetInstance().SetConsoleSuppressed(false);
}

void Cmd::RunFunc(void *ctx)
{
    Cmd* cmd = static_cast<Cmd*>(ctx);
    cmd->Run();
}

void Cmd::OnChar(char c, u8 code)
{
    /* The editor reads the character: the keyboards send a terminal's
       bytes, the serial ports are one */
    (void)code;

    Stdlib::AutoLock lock(Lock);
    if (!Active)
        return;

    if (!Buf.Put(c))
    {
        Trace(0, "Can't save char");
        return;
    }
}

}
