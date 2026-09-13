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
#include "disklog.h"
#include <block/block_device.h>
#include <block/partition.h>
#include "parameters.h"
#include <net/net_device.h>
#include <net/net_frame_pool.h>
#include <net/net.h>
#include <net/arp.h>
#include <net/dhcp.h>
#include <net/icmp.h>
#include <net/dns.h>
#include <net/tcp.h>
#include <net/http.h>
#include <net/netconsole.h>
#include <fs/vfs.h>
#include <fs/ramfs.h>
#include <fs/nanofs.h>
#include <fs/ext2.h>
#include <fs/fstest.h>
#include "entropy.h"
#include "random.h"
#include "console.h"
#include "mutex.h"
#include "task.h"
#include "stack_probe.h"
#include <net/net_load.h>
#include "stack_trace.h"
#include "symtab.h"
#include "profiler.h"
#include "module.h"

#include <drivers/vga.h>
#include <drivers/pci.h>
#ifdef __x86_64__
#include <drivers/usb/xhci.h>
#endif
#include <include/const.h>
#include <mm/page_table.h>
#include <mm/memory_map.h>
#include <mm/new.h>
#include <lib/unique_ptr.h>
#include <lib/checksum.h>
#include <lib/grub_env.h>
#include "sha256.h"

namespace Kernel
{

static DhcpClient& GetDhcpClient()
{
    static DhcpClient instance;
    return instance;
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

    con.Printf("cpus %u running, id mask 0x%p, bsp %u\n",
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
        con.Printf("loglevel %u\n", (ulong)tracer.GetLevel());
        return;
    }

    char buf[16];
    Stdlib::TokenCopy(tok, end, buf, sizeof(buf));

    ulong level;
    if (!Stdlib::ParseUlong(buf, level) || level > (ulong)MaxTraceLevel)
    {
        con.Printf("usage: loglevel [0-%u]\n", (ulong)MaxTraceLevel);
        return;
    }

    /* The per-subsystem levels in trace.h are compile-time constants, so the
       only way to see what xHCI or the page allocator is doing used to be a
       rebuild and a reflash -- on a machine whose console is a UDP socket that
       is most of an afternoon. Raising the level is loud: level 4 traces every
       allocation, and every line also goes out the serial port. */
    tracer.SetLevel((int)level);
    con.Printf("loglevel %u\n", level);
}

static void CmdUptime(const char* args, Stdlib::Printer& con)
{
    (void)args;
    auto time = GetBootTime();
    con.Printf("%u.%u\n", time.GetSecs(), time.GetUsecs());
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

    con.Printf("%u-%u-%u %u:%u:%u UTC\n", y, mo, d, h, m, s);
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
            con.Printf("usage: top [interval-ms, 1-%u]\n", MaxIntervalMs);
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

    Sleep(intervalMs * Const::NanoSecsInMs);

    size_t n1 = table.SampleCpu(after, MaxSamples);
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

    con.Printf("cpus %u, %u tasks, %u ms window, busy %u.%u%% of %u00%%\n",
        cpus, (ulong)n1, intervalMs, busy / 10, busy % 10, cpus);
    con.Printf("  pid    cpu%%  name\n");

    for (size_t i = 0; i < n1; i++)
    {
        con.Printf("%u %u.%u %s\n", after[i].Pid,
            permille[i] / 10, permille[i] % 10, after[i].Name);
    }

    Mm::Free(after);
    Mm::Free(before);

    con.Printf("task migrations since boot: %u\n", (ulong)GetTaskMigrationCount());
    con.Printf("preemptions deferred since boot: %u, %u of them on an idle task\n",
        (ulong)GetPreemptDeferredCount(), (ulong)GetPreemptDeferredIdleCount());
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

    static const char* Usage =
        "usage: profile [ms, 1-%u] [pid|all] [chains]\n";

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

    con.Printf("profiled %u ms", ms);
    if (pid != Profiler::NoPidFilter)
        con.Printf(", pid %u only", pid);
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

    con.Printf("closest any stack came to its end: %u bytes free\n", worstFree);
}


/* Mirrors R8125State in src/rust/drivers/r8125/src/lib.rs. */
struct R8125State
{
    u32 Present;
    u32 Cmd;
    u32 IntrStatus;
    u32 IntrMask;
    u32 RxConfig;
    u32 RxHead;
    u32 HeadPosted;
    u32 HeadOpts1;
    u64 RxErrEvents;
    u64 RxPolls;
    u64 RxBudgetHits;
    u64 RxPackets;
    u64 RxDropped;
};

extern "C" int r8125_get_state(R8125State* out);

/* Mirrors IgbState in src/rust/drivers/igb/src/lib.rs. */
struct IgbState
{
    u32 Present;
    u32 Generation;
    u32 PhyBmcr;
    u32 PhyBmsr;
    u32 PhyAnar;
    u32 PhyAnlpar;
    u32 PhyGctl;
    u32 PhyGstat;
    u32 Ctrl;
    u32 Status;
    u32 Rctl;
    u32 Tctl;
    u32 Ims;
    u32 Eitr;
    u64 StatTpr;
    u64 StatGprc;
    u64 StatMpc;
    u64 StatRnbc;
    u64 StatRxerrc;
    u32 StatRqdpc;
    u32 StatPqgprc;
    u32 Rxdctl;
    u32 Srrctl;
    u32 Rdh;
    u32 Rdt;
    u32 Tdh;
    u32 Tdt;
    u32 NextToClean;
    u32 NextToUse;
    u32 HeadStatus;
    u32 HeadPosted;
    u64 RxPolls;
    u64 RxBudgetHits;
    u64 RxErrEvents;
    u64 RxPackets;
    u64 RxDropped;
    u64 TxPackets;
};

extern "C" int igb_get_state(IgbState* out);

static void CmdDisklog(const char* args, Stdlib::Printer& con)
{
    (void)args;
    DiskLog::GetInstance().Dump(con);
}

static void CmdIgbdump(const char* args, Stdlib::Printer& con)
{
    (void)args;

    IgbState st;
    Stdlib::MemSet(&st, 0, sizeof(st));

    if (igb_get_state(&st) != 0 || st.Present == 0)
    {
        con.Printf("igbdump: no igb\n");
        return;
    }

    static const u32 StatusLu = 1u << 1;
    static const u32 RctlEn = 1u << 1;
    static const u32 TctlEn = 1u << 1;
    static const u32 XdctlEnable = 1u << 25;
    static const u32 RxdStatDd = 1u << 0;

    static const u32 BmsrLstatus = 1u << 2;
    static const u32 BmsrAnegDone = 1u << 5;

    con.Printf("part %s\n", st.Generation ? "I210" : "82576");
    con.Printf("ctrl 0x%p status 0x%p link %u\n", (ulong)st.Ctrl, (ulong)st.Status,
        (ulong)((st.Status & StatusLu) ? 1 : 0));
    /* The PHY's own view, which is what tells a link the driver never brought
       up apart from a cable that is not plugged in. */
    con.Printf("phy bmcr 0x%p bmsr 0x%p link %u autoneg-done %u\n",
        (ulong)st.PhyBmcr, (ulong)st.PhyBmsr,
        (ulong)((st.PhyBmsr & BmsrLstatus) ? 1 : 0),
        (ulong)((st.PhyBmsr & BmsrAnegDone) ? 1 : 0));
    /* What we offered against what came back: a link that resolves lower than
       it should is one or the other, and nothing else distinguishes them. */
    con.Printf("phy adv 0x%p partner 0x%p  1000: ctrl 0x%p status 0x%p\n",
        (ulong)st.PhyAnar, (ulong)st.PhyAnlpar,
        (ulong)st.PhyGctl, (ulong)st.PhyGstat);
    con.Printf("rctl 0x%p rx-en %u  tctl 0x%p tx-en %u  ims 0x%p\n",
        (ulong)st.Rctl, (ulong)((st.Rctl & RctlEn) ? 1 : 0),
        (ulong)st.Tctl, (ulong)((st.Tctl & TctlEn) ? 1 : 0), (ulong)st.Ims);
    /* Microseconds the chip holds interrupts apart. Firmware leaves a value
       here that a device reset does not clear, and it caps the receive rate
       on its own. */
    con.Printf("interrupt throttle %u us\n", (ulong)st.Eitr);
    /* The prefetch thresholds live in the low fields of RXDCTL. Zero there
       means the chip never prefetches descriptors and drops packets with a
       full ring, so they are worth reading back rather than assuming. */
    con.Printf("rxdctl 0x%p queue-en %u (pthresh %u hthresh %u wthresh %u) srrctl 0x%p\n",
        (ulong)st.Rxdctl, (ulong)((st.Rxdctl & XdctlEnable) ? 1 : 0),
        (ulong)(st.Rxdctl & 0x1F), (ulong)((st.Rxdctl >> 8) & 0x1F),
        (ulong)((st.Rxdctl >> 16) & 0x1F), (ulong)st.Srrctl);
    con.Printf("rx ring: rdh %u rdt %u  clean %u use %u\n",
        (ulong)st.Rdh, (ulong)st.Rdt, (ulong)st.NextToClean, (ulong)st.NextToUse);
    con.Printf("rx head: status 0x%p dd %u posted %u\n", (ulong)st.HeadStatus,
        (ulong)((st.HeadStatus & RxdStatDd) ? 1 : 0), (ulong)st.HeadPosted);
    con.Printf("tx ring: tdh %u tdt %u packets %u\n",
        (ulong)st.Tdh, (ulong)st.Tdt, (ulong)st.TxPackets);
    con.Printf("rx packets %u dropped %u err events %u\n",
        (ulong)st.RxPackets, (ulong)st.RxDropped, (ulong)st.RxErrEvents);
    con.Printf("rx polls %u, of them budget-limited %u\n",
        (ulong)st.RxPolls, (ulong)st.RxBudgetHits);
    /* The chip's own view, which the ring counters cannot see: a frame the MAC
       dropped before it reached for a descriptor never appears above. TPR is
       everything taken off the wire, MPC what the receive FIFO had no room
       for, RNBC what found no descriptor waiting. */
    con.Printf("mac: total %u good %u missed %u no-buffer %u errors %u\n",
        (ulong)st.StatTpr, (ulong)st.StatGprc, (ulong)st.StatMpc,
        (ulong)st.StatRnbc, (ulong)st.StatRxerrc);
    /* Per queue. RQDPC counts packets the queue was offered and had no
       descriptor for -- the one drop nothing else in this dump can see. */
    con.Printf("queue0: good %u dropped-no-descriptor %u\n",
        (ulong)st.StatPqgprc, (ulong)st.StatRqdpc);
}

static void CmdNicdump(const char* args, Stdlib::Printer& con)
{
    (void)args;

    R8125State st;
    Stdlib::MemSet(&st, 0, sizeof(st));

    if (r8125_get_state(&st) != 0 || st.Present == 0)
    {
        con.Printf("nicdump: no r8125\n");
        return;
    }

    /* Bit 3 of the command register is the receiver. If it is clear the chip
       has switched itself off and nothing the driver does to descriptors will
       bring it back -- which is the one thing six guesses about this stall
       never checked. */
    static const u32 CmdRxEn = 0x08;
    static const u32 CmdTxEn = 0x04;
    static const u32 RxOwn = 0x80000000;

    con.Printf("cmd 0x%p rx-en %u tx-en %u\n", (ulong)st.Cmd,
        (ulong)((st.Cmd & CmdRxEn) ? 1 : 0), (ulong)((st.Cmd & CmdTxEn) ? 1 : 0));
    con.Printf("isr 0x%p imr 0x%p rxcfg 0x%p\n",
        (ulong)st.IntrStatus, (ulong)st.IntrMask, (ulong)st.RxConfig);
    con.Printf("rx head %u posted %u opts1 0x%p own %u\n",
        (ulong)st.RxHead, (ulong)st.HeadPosted, (ulong)st.HeadOpts1,
        (ulong)((st.HeadOpts1 & RxOwn) ? 1 : 0));
    con.Printf("rx packets %u dropped %u err events %u\n",
        (ulong)st.RxPackets, (ulong)st.RxDropped, (ulong)st.RxErrEvents);

    /* A ceiling that reads as polls-per-second times budget is either the
       softirq loop's cadence or the chip's interrupt rate; these tell which. */
    con.Printf("rx polls %u, of them budget-limited %u\n",
        (ulong)st.RxPolls, (ulong)st.RxBudgetHits);
}

static void CmdNetload(const char* args, Stdlib::Printer& con)
{
    auto& load = NetLoad::GetInstance();

    const char* end;
    const char* tok = Stdlib::NextToken(args, end);

    if (tok == nullptr)
    {
        load.Dump(con);
        return;
    }

    char buf[16];
    Stdlib::TokenCopy(tok, end, buf, sizeof(buf));

    if (Stdlib::StrCmp(buf, "stop") == 0)
    {
        if (!load.IsRunning())
        {
            con.Printf("netload: not running\n");
            return;
        }

        load.Stop();
        con.Printf("netload: stopped\n");
        return;
    }

    if (Stdlib::StrCmp(buf, "reset") == 0)
    {
        load.ResetCounters();
        con.Printf("netload: counters cleared\n");
        return;
    }

    if (Stdlib::StrCmp(buf, "start") != 0)
    {
        con.Printf("usage: netload [start [port] [sink] | stop | reset]\n");
        return;
    }

    if (load.IsRunning())
    {
        con.Printf("netload: already running\n");
        return;
    }

    ulong port = NetLoad::DefaultPort;
    bool echo = true;

    tok = Stdlib::NextToken(end, end);
    if (tok != nullptr)
    {
        Stdlib::TokenCopy(tok, end, buf, sizeof(buf));

        if (Stdlib::StrCmp(buf, "sink") == 0)
        {
            echo = false;
        }
        else
        {
            if (!Stdlib::ParseUlong(buf, port) || port == 0 || port > 65535)
            {
                con.Printf("usage: netload [start [port] [sink] | stop | reset]\n");
                return;
            }

            tok = Stdlib::NextToken(end, end);
            if (tok != nullptr)
            {
                Stdlib::TokenCopy(tok, end, buf, sizeof(buf));
                if (Stdlib::StrCmp(buf, "sink") == 0)
                    echo = false;
            }
        }
    }

    NetDevice* dev = NetDeviceTable::GetInstance().Find("eth0");
    if (dev == nullptr)
    {
        con.Printf("netload: no eth0\n");
        return;
    }

    if (!load.Start(dev, (u16)port, echo))
    {
        con.Printf("netload: could not start on port %u\n", port);
        return;
    }

    con.Printf("netload: listening on udp %u, %s\n", port, echo ? "echo" : "sink");
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

    con.Printf("freePages: %u\n", pt.GetFreePagesCount());
    con.Printf("totalPages: %u\n", pt.GetTotalPagesCount());
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

        con.Printf("0x%p 0x%p %s\n", addr, len,
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

    con.Printf("usable: %u MiB in %u regions\n", usable / Const::MB, usableRegions);
    con.Printf("mapped: %u MiB, bootstrap map ends at 0x%p\n",
        (usable - unused) / Const::MB, mapLimit);
    if (unused != 0)
        con.Printf("unused: %u MiB above the bootstrap map\n", unused / Const::MB);
    con.Printf("pages: %u free of %u\n",
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
            con.Printf("%s: %u\n", InterruptStats::GetName(src), count);
    }
}

static void CmdPci(const char* args, Stdlib::Printer& con)
{
    (void)args;
    Pci::GetInstance().Dump(con);
}

static void CmdUsb(const char* args, Stdlib::Printer& con)
{
    (void)args;
#ifdef __x86_64__
    Usb::Controller::Dump(con);
#else
    con.Printf("usb: not supported on this architecture\n");
#endif
}

static void CmdDisks(const char* args, Stdlib::Printer& con)
{
    (void)args;
    BlockDeviceTable::GetInstance().Dump(con);
}

static void CmdPartitions(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* nameStart = Stdlib::NextToken(args, end);
    if (!nameStart)
    {
        con.Printf("usage: partitions <disk>\n");
        return;
    }

    char diskName[16];
    Stdlib::TokenCopy(nameStart, end, diskName, sizeof(diskName));

    BlockDevice* dev = BlockDeviceTable::GetInstance().Find(diskName);
    if (!dev)
    {
        con.Printf("disk '%s' not found\n", diskName);
        return;
    }

    /* ReadSectors transfers a full hardware sector (may exceed 512, e.g.
       4K-LBA NVMe), so size the buffer from the device, not the MBR. */
    Stdlib::UniquePtr<u8, Mm::FreeDeleter> bufPtr(
        static_cast<u8*>(Mm::Alloc(dev->GetSectorSize(), 0)));
    if (!bufPtr.Get())
    {
        con.Printf("alloc failed\n");
        return;
    }
    u8* buf = bufPtr.Get();

    if (!dev->ReadSectors(0, buf, 1))
    {
        con.Printf("failed to read sector 0\n");
        return;
    }

    auto* mbr = reinterpret_cast<Mbr*>(buf);
    if (mbr->Signature != Mbr::ValidSignature)
    {
        con.Printf("no MBR partition table (signature 0x%p)\n", (ulong)mbr->Signature);
        return;
    }

    con.Printf("  #  Type  LBA Start   LBA Size    Size\n");
    for (ulong i = 0; i < Mbr::MaxParts; i++)
    {
        auto& entry = mbr->Parts[i];
        if (entry.Type == 0 && entry.LbaSize == 0)
            continue;

        u64 sizeBytes = (u64)entry.LbaSize * dev->GetSectorSize();
        u64 mb = sizeBytes / (1024 * 1024);

        con.Printf("  %u  0x%p  %u  %u  %u MB\n",
            i + 1, (ulong)entry.Type,
            (ulong)entry.LbaStart, (ulong)entry.LbaSize, mb);
    }
}

static void CmdDiskread(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* nameStart = Stdlib::NextToken(args, end);
    if (!nameStart)
    {
        con.Printf("usage: diskread <disk> <sector>\n");
        return;
    }

    char diskName[16];
    Stdlib::TokenCopy(nameStart, end, diskName, sizeof(diskName));

    const char* secStart = Stdlib::NextToken(end, end);
    ulong sector = 0;
    if (!secStart)
    {
        con.Printf("usage: diskread <disk> <sector>\n");
        return;
    }

    char secBuf[32];
    Stdlib::TokenCopy(secStart, end, secBuf, sizeof(secBuf));

    if (!Stdlib::ParseUlong(secBuf, sector))
    {
        con.Printf("invalid sector number\n");
        return;
    }

    BlockDevice* dev = BlockDeviceTable::GetInstance().Find(diskName);
    if (!dev)
    {
        con.Printf("disk '%s' not found\n", diskName);
        return;
    }

    Stdlib::UniquePtr<u8, Mm::FreeDeleter> bufPtr(static_cast<u8*>(Mm::Alloc(Const::PageSize, 0)));
    if (!bufPtr.Get())
    {
        con.Printf("alloc failed\n");
        return;
    }
    u8* buf = bufPtr.Get();

    if (!dev->ReadSectors(sector, buf, 1))
    {
        con.Printf("read error\n");
        return;
    }

    for (ulong i = 0; i < 512; i += 16)
    {
        con.Printf("%p: ", sector * 512 + i);
        for (ulong j = 0; j < 16 && (i + j) < 512; j++)
        {
            con.Printf("%p ", (ulong)buf[i + j]);
        }
        con.Printf("\n");
    }
}

/* What diskwrite's claim on its device says to whoever is refused it */
static const char DiskwriteHolder[] = "diskwrite";

static void CmdDiskwrite(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* nameStart = Stdlib::NextToken(args, end);
    if (!nameStart)
    {
        con.Printf("usage: diskwrite <disk> <sector> <hex>\n");
        return;
    }

    char diskName[16];
    Stdlib::TokenCopy(nameStart, end, diskName, sizeof(diskName));

    const char* secStart = Stdlib::NextToken(end, end);
    ulong sector = 0;
    if (!secStart)
    {
        con.Printf("usage: diskwrite <disk> <sector> <hex>\n");
        return;
    }

    char secBuf[32];
    Stdlib::TokenCopy(secStart, end, secBuf, sizeof(secBuf));

    const char* hexStart = Stdlib::NextToken(end, end);
    if (!Stdlib::ParseUlong(secBuf, sector) || !hexStart)
    {
        con.Printf("usage: diskwrite <disk> <sector> <hex>\n");
        return;
    }

    BlockDevice* dev = BlockDeviceTable::GetInstance().Find(diskName);
    if (!dev)
    {
        con.Printf("disk '%s' not found\n", diskName);
        return;
    }

    Stdlib::UniquePtr<u8, Mm::FreeDeleter> bufPtr(static_cast<u8*>(Mm::Alloc(Const::PageSize, 0)));
    if (!bufPtr.Get())
    {
        con.Printf("alloc failed\n");
        return;
    }
    u8* buf = bufPtr.Get();

    Stdlib::MemSet(buf, 0, Const::PageSize);
    ulong hexLen = (ulong)(end - hexStart);
    ulong byteCount = 0;
    if (!Stdlib::HexDecode(hexStart, hexLen, buf, 512, byteCount))
    {
        con.Printf("invalid hex data\n");
        return;
    }

    if (byteCount > 0)
    {
        /* Not around a mounted filesystem, the disk log or a write test */
        auto& table = BlockDeviceTable::GetInstance();
        const char* heldBy = nullptr;
        const ulong claim = table.Claim(dev, DiskwriteHolder, heldBy);
        if (claim == 0)
        {
            con.Printf("disk '%s' is in use by %s\n", diskName, heldBy);
            return;
        }

        if (!dev->WriteSectors(sector, buf, 1))
            con.Printf("write error\n");
        else
            con.Printf("wrote %u bytes to sector %u\n", byteCount, sector);

        table.Release(claim);
    }
}

static void CmdNet(const char* args, Stdlib::Printer& con)
{
    (void)args;
    NetDeviceTable::GetInstance().Dump(con);
    con.Printf("rx polls %u, poll work %u, stalls %u\n",
        NetDeviceTable::GetInstance().GetRxPolls(),
        NetDeviceTable::GetInstance().GetRxPollWork(),
        NetDeviceTable::GetInstance().GetRxStalls());
}

static void CmdNetpool(const char* args, Stdlib::Printer& con)
{
    (void)args;
    NetFramePool::GetInstance().Dump(con);
}

static void CmdArp(const char* args, Stdlib::Printer& con)
{
    (void)args;
    ArpTable::GetInstance().Dump(con);
}

static void CmdNetconsole(const char* args, Stdlib::Printer& con)
{
    (void)args;
    Netconsole::GetInstance().Dump(con);
}

static void CmdIcmpstat(const char* args, Stdlib::Printer& con)
{
    (void)args;
    Icmp::GetInstance().Dump(con);
}

static void CmdTcpstat(const char* args, Stdlib::Printer& con)
{
    (void)args;
    Tcp::GetInstance().Dump(con);
}

/* The body is gathered into blocks this size before each write: ext2 then
   commits its metadata once per block rather than once per TCP segment. */
static const ulong WgetWriteBufSize = 64 * 1024;
/* Progress line every this many bytes; a big download over a slow link
   otherwise looks like a hang. */
static const ulong WgetReportStep = 1024 * 1024;
/* The longest URL a command line can carry (the UDP shell takes 255-byte
   commands, the console 80). Redirect targets run far longer -- a GitHub
   release link becomes ~900 characters -- but those never pass through
   here: the client keeps them in its own HttpMaxUrlLen buffers. */
static const ulong WgetMaxUrlLen = 256;

/* Streams a download straight to a file. The body never exists in memory:
   it arrives in TCP-sized pieces and leaves in WgetWriteBufSize blocks, so
   a 20 MB file costs one 64 KB buffer. */
class WgetFileSink : public HttpSink
{
public:
    WgetFileSink(File* file, Stdlib::Printer& con)
        : Out(file)
        , Con(con)
        , Buf(nullptr)
        , Used(0)
        , Written(0)
        , Reported(0)
    {
    }

    virtual ~WgetFileSink()
    {
        if (Buf != nullptr)
            Mm::Free(Buf);
    }

    bool Setup()
    {
        Buf = (u8*)Mm::Alloc(WgetWriteBufSize, 'Wget');
        return Buf != nullptr;
    }

    virtual ulong Write(const u8* data, ulong len) override
    {
        ulong before = Written;
        ulong taken = 0;

        while (taken < len)
        {
            ulong room = WgetWriteBufSize - Used;
            ulong take = (len - taken < room) ? (len - taken) : room;

            Stdlib::MemCpy(Buf + Used, data + taken, take);
            Used += take;
            taken += take;

            /* A failed block never reached the disk, and neither did
               anything still buffered: only what Flush committed counts. */
            if (Used == WgetWriteBufSize && !Flush())
                return Written - before;
        }

        ulong total = Written + Used;
        if (total - Reported >= WgetReportStep)
        {
            Reported = total - (total % WgetReportStep);
            Con.Printf("wget: %u KB\n", total / Const::KB);
        }

        return taken;
    }

    /* Pushes what the buffer still holds; call once the body is over. */
    bool Flush()
    {
        if (Used == 0)
            return true;

        ulong len = Used;
        Used = 0;

        if (!Vfs::GetInstance().Write(Out, Buf, len))
        {
            Con.Printf("wget: write failed after %u bytes\n", Written);
            return false;
        }

        Written += len;
        return true;
    }

    /* Bytes committed to the file. */
    ulong GetTotal() const { return Written; }

private:
    WgetFileSink(const WgetFileSink& other) = delete;
    WgetFileSink& operator=(const WgetFileSink& other) = delete;

    File* Out;
    Stdlib::Printer& Con;
    u8* Buf;
    ulong Used;      /* bytes buffered, not yet written */
    ulong Written;   /* bytes committed to the file */
    ulong Reported;
};

/* Why the request produced nothing. TLS gets its own line: "failed" for a
   rejected certificate would send the reader looking in the wrong place. */
static void WgetPrintFailure(const HttpResponse& resp, Stdlib::Printer& con)
{
    if (resp.TlsFailed)
        con.Printf("wget: TLS handshake refused -- bad certificate, or no "
                   "protocol in common (dmesg has the reason)\n");
    else if (resp.Err.GetCode() == Stdlib::Error::BufTooBig)
        con.Printf("wget: URL, or a redirect's target, longer than %u "
                   "characters\n", HttpMaxUrlLen - 1);
    else
        con.Printf("wget: failed\n");
}

/* Downloads to a file, streaming. Returns false with the reason printed. */
static bool WgetToFile(NetDevice* dev, const char* url, const char* path,
                       Stdlib::Printer& con)
{
    File* file = Vfs::GetInstance().Open(path,
        Vfs::OpenWrite | Vfs::OpenCreate | Vfs::OpenTruncate);
    if (file == nullptr)
    {
        con.Printf("wget: cannot open %s for writing\n", path);
        return false;
    }

    WgetFileSink sink(file, con);
    if (!sink.Setup())
    {
        con.Printf("wget: out of memory\n");
        Vfs::GetInstance().Close(file);
        return false;
    }

    HttpClient client(dev);
    HttpResponse resp = client.Get(url, sink);

    bool flushed = sink.Flush();
    Vfs::GetInstance().Close(file);

    /* Nothing landed -- a failed request, or a body refused before the
       first byte: do not leave an empty file behind. */
    if (sink.GetTotal() == 0)
        Vfs::GetInstance().Remove(path);

    if (!resp.Ok)
    {
        WgetPrintFailure(resp, con);
        return false;
    }

    con.Printf("HTTP %u, %u bytes\n", (ulong)resp.StatusCode, resp.BodyLen);

    if (resp.Location[0] != '\0')
        con.Printf("Location: %s\n", resp.Location);

    if (!flushed)
        return false;

    if (resp.Truncated)
    {
        if (resp.Err.GetCode() == Stdlib::Error::BufTooBig)
            con.Printf("wget: body over the %u MB limit\n",
                       (ulong)(HttpMaxBodySize / Const::MB));
        else
            con.Printf("wget: incomplete, %u bytes saved to %s\n",
                       sink.GetTotal(), path);
        return false;
    }

    con.Printf("saved %u bytes to %s\n", sink.GetTotal(), path);
    return true;
}

static void CmdWget(const char* args, Stdlib::Printer& con)
{
    char url[WgetMaxUrlLen];
    char path[Vfs::MaxPath];
    url[0] = '\0';
    path[0] = '\0';

    /* wget [-o <path>] <url> [path] -- the flag and the trailing argument
       mean the same thing, whichever reads better. */
    const char* end = args;
    for (const char* tok = Stdlib::NextToken(args, end); tok != nullptr;
         tok = Stdlib::NextToken(end, end))
    {
        char arg[Vfs::MaxPath];
        Stdlib::TokenCopy(tok, end, arg, sizeof(arg));

        if (Stdlib::StrCmp(arg, "-o") == 0)
        {
            const char* out = Stdlib::NextToken(end, end);
            if (out == nullptr)
            {
                con.Printf("wget: -o needs a path\n");
                return;
            }
            Stdlib::TokenCopy(out, end, path, sizeof(path));
        }
        else if (url[0] == '\0')
        {
            /* A cut-down URL would fetch some other resource. */
            if (Stdlib::TokenCopy(tok, end, url, sizeof(url)) <
                (ulong)(end - tok))
            {
                con.Printf("wget: URL longer than %u characters\n",
                           WgetMaxUrlLen - 1);
                return;
            }
        }
        else if (path[0] == '\0')
        {
            Stdlib::TokenCopy(tok, end, path, sizeof(path));
        }
    }

    if (url[0] == '\0')
    {
        con.Printf("usage: wget [-o <path>] <url> [path]\n");
        return;
    }

    NetDevice* dev = NetDeviceTable::GetInstance().Find("eth0");
    if (!dev)
    {
        con.Printf("eth0 not found\n");
        return;
    }

    if (path[0] != '\0')
    {
        WgetToFile(dev, url, path, con);
        return;
    }

    /* No file: the body is kept in memory, capped at HttpMaxResponseSize. */
    HttpClient client(dev);
    HttpResponse resp = client.Get(url);

    if (!resp.Ok)
    {
        WgetPrintFailure(resp, con);
        return;
    }

    con.Printf("HTTP %u, %u bytes\n", (ulong)resp.StatusCode, resp.BodyLen);

    if (resp.Location[0] != '\0')
        con.Printf("Location: %s\n", resp.Location);

    if (resp.Body && resp.BodyLen > 0)
    {
        /* Print body as text, truncate to 4 KB for display */
        static const ulong MaxDisplay = 4096;
        ulong displayLen = resp.BodyLen;
        if (displayLen > MaxDisplay)
            displayLen = MaxDisplay;
        for (ulong i = 0; i < displayLen; i++)
            con.Printf("%c", (ulong)resp.Body[i]);
        con.Printf("\n");
        if (resp.BodyLen > MaxDisplay)
            con.Printf("... (%u bytes truncated)\n", resp.BodyLen - MaxDisplay);
    }

    if (resp.Truncated)
        con.Printf("wget: body truncated, pass a path to save it to a file\n");

    if (resp.Body)
        Mm::Free(resp.Body);
}

static void CmdUdpsend(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* ipStart = Stdlib::NextToken(args, end);
    if (!ipStart)
    {
        con.Printf("usage: udpsend <ip> <port> <message>\n");
        return;
    }

    char ipBuf[16];
    Stdlib::TokenCopy(ipStart, end, ipBuf, sizeof(ipBuf));

    const char* portStart = Stdlib::NextToken(end, end);
    if (!portStart)
    {
        con.Printf("usage: udpsend <ip> <port> <message>\n");
        return;
    }

    char portBuf[8];
    Stdlib::TokenCopy(portStart, end, portBuf, sizeof(portBuf));

    ulong port = 0;
    if (!Stdlib::ParseUlong(portBuf, port) || port > 65535)
    {
        con.Printf("invalid port\n");
        return;
    }

    /* Skip whitespace to get message */
    const char* msg = end;
    while (*msg == ' ')
        msg++;

    if (*msg == '\0')
    {
        con.Printf("usage: udpsend <ip> <port> <message>\n");
        return;
    }

    Net::IpAddress dstIp;
    if (!Net::IpAddress::Parse(ipBuf, dstIp))
    {
        con.Printf("invalid IP '%s'\n", ipBuf);
        return;
    }

    /* Find first net device */
    NetDevice* dev = nullptr;
    if (NetDeviceTable::GetInstance().GetCount() > 0)
        dev = NetDeviceTable::GetInstance().Find("eth0");

    if (!dev)
    {
        con.Printf("no network device\n");
        return;
    }

    ulong msgLen = Stdlib::StrLen(msg);
    Net::IpAddress srcIp = dev->GetIp();

    /* Resolve destination MAC via ARP */
    Net::MacAddress dstMac;
    if (!ArpTable::GetInstance().Resolve(dev, dstIp, dstMac))
        dstMac = Net::MacAddress::Broadcast();

    /* Build UDP frame */
    ulong udpPayLen = sizeof(Net::UdpHdr) + msgLen;
    ulong ipPayLen = sizeof(Net::IpHdr) + udpPayLen;
    ulong frameLen = sizeof(Net::EthHdr) + ipPayLen;

    if (frameLen > 1514)
    {
        con.Printf("message too large\n");
        return;
    }

    u8 frame[1514];
    Stdlib::MemSet(frame, 0, sizeof(frame));
    ulong off = 0;

    Net::EthHdr* eth = (Net::EthHdr*)(frame + off);
    dstMac.CopyTo(eth->DstMac);
    dev->GetMac().CopyTo(eth->SrcMac);
    eth->EtherType = Net::Htons(Net::EtherTypeIp);
    off += sizeof(Net::EthHdr);

    Net::IpHdr* ip = (Net::IpHdr*)(frame + off);
    ip->VersionIhl = 0x45;
    ip->TotalLen = Net::Htons((u16)ipPayLen);
    ip->Ttl = 64;
    ip->Protocol = Net::IpProtoUdp;
    ip->SrcAddr = srcIp.ToNetwork();
    ip->DstAddr = dstIp.ToNetwork();
    ip->Checksum = Net::Htons(Net::IpChecksum(ip, sizeof(Net::IpHdr)));
    off += sizeof(Net::IpHdr);

    Net::UdpHdr* udp = (Net::UdpHdr*)(frame + off);
    udp->SrcPort = Net::Htons(12345);
    udp->DstPort = Net::Htons((u16)port);
    udp->Length = Net::Htons((u16)udpPayLen);
    off += sizeof(Net::UdpHdr);

    Stdlib::MemCpy(frame + off, msg, msgLen);
    off += msgLen;

    if (dev->SendRaw(frame, off))
    {
        con.Printf("sent %u bytes to %s:%u\n",
            msgLen, ipBuf, port);
    }
    else
    {
        con.Printf("send failed\n");
    }
}

static void CmdPing(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* ipStart = Stdlib::NextToken(args, end);
    if (!ipStart)
    {
        con.Printf("usage: ping <ip|hostname>\n");
        return;
    }

    char hostBuf[DnsResolver::MaxDomainLen + 1];
    Stdlib::TokenCopy(ipStart, end, hostBuf, sizeof(hostBuf));

    Net::IpAddress dstIp;
    if (!Net::IpAddress::Parse(hostBuf, dstIp))
    {
        if (!DnsResolver::GetInstance().IsInitialized() ||
            !DnsResolver::GetInstance().Resolve(hostBuf, dstIp))
        {
            con.Printf("cannot resolve '%s'\n", hostBuf);
            return;
        }
    }

    NetDevice* dev = nullptr;
    if (NetDeviceTable::GetInstance().GetCount() > 0)
        dev = NetDeviceTable::GetInstance().Find("eth0");

    if (!dev)
    {
        con.Printf("no network device\n");
        return;
    }

    u16 pingId = (u16)(Hal::ReadCycleCounter() & 0xFFFF);

    con.Printf("PING %s\n", hostBuf);
    ulong received = 0;

    for (u16 seq = 0; seq < 5; seq++)
    {
        if (!Icmp::GetInstance().SendEchoRequest(dev, dstIp, pingId, seq))
        {
            con.Printf("send failed seq=%u\n", (ulong)seq);
        }
        else
        {
            ulong rttNs = 0;
            if (Icmp::GetInstance().WaitReply(pingId, seq, 3000, rttNs))
            {
                ulong rttMs = rttNs / Const::NanoSecsInMs;
                con.Printf("reply from %s: seq=%u time=%u ms\n",
                    hostBuf, (ulong)seq, rttMs);
                received++;
            }
            else
            {
                con.Printf("request timeout seq=%u\n", (ulong)seq);
            }
        }

        if (seq < 4)
            Sleep(1000 * Const::NanoSecsInMs);
    }

    con.Printf("%u/5 received\n", received);
}

static void CmdNslookup(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* nameStart = Stdlib::NextToken(args, end);
    if (!nameStart)
    {
        con.Printf("usage: nslookup <hostname>\n");
        return;
    }

    char hostBuf[DnsResolver::MaxDomainLen + 1];
    Stdlib::TokenCopy(nameStart, end, hostBuf, sizeof(hostBuf));

    if (!DnsResolver::GetInstance().IsInitialized())
    {
        con.Printf("DNS resolver not initialized\n");
        return;
    }

    Net::IpAddress ip;
    if (DnsResolver::GetInstance().Resolve(hostBuf, ip))
    {
        con.Printf("%s -> %u.%u.%u.%u\n", hostBuf,
            (ulong)((ip.Addr4 >> 24) & 0xFF),
            (ulong)((ip.Addr4 >> 16) & 0xFF),
            (ulong)((ip.Addr4 >> 8) & 0xFF),
            (ulong)(ip.Addr4 & 0xFF));
    }
    else
    {
        con.Printf("failed to resolve '%s'\n", hostBuf);
    }
}

static void CmdDnsflush(const char* args, Stdlib::Printer& con)
{
    (void)args;
    DnsResolver::GetInstance().Flush();
    con.Printf("dns cache flushed\n");
}

static void CmdDhcp(const char* args, Stdlib::Printer& con)
{
    if (Parameters::GetInstance().IsDhcpOff())
    {
        con.Printf("DHCP disabled (dhcp=off)\n");
        return;
    }

    static Mutex dhcpLock;
    Stdlib::AutoLock lock(dhcpLock);

    const char* devName = "eth0";
    if (args[0] != '\0')
        devName = args;

    NetDevice* dev = NetDeviceTable::GetInstance().Find(devName);
    if (!dev)
    {
        con.Printf("device '%s' not found\n", devName);
        return;
    }

    if (GetDhcpClient().IsReady())
    {
        DhcpResult r = GetDhcpClient().GetResult();
        con.Printf("already bound: ");
        r.Ip.Print(con);
        con.Printf("\n");
        return;
    }

    con.Printf("DHCP discovering on %s...\n", devName);
    if (!GetDhcpClient().Start(dev))
    {
        con.Printf("failed to start DHCP\n");
        return;
    }

    /* Wait up to 10 seconds for a lease */
    for (ulong i = 0; i < 100 && !GetDhcpClient().IsReady(); i++)
        Sleep(100 * Const::NanoSecsInMs);

    if (GetDhcpClient().IsReady())
    {
        DhcpResult r = GetDhcpClient().GetResult();
        con.Printf("ip:     "); r.Ip.Print(con); con.Printf("\n");
        con.Printf("mask:   "); r.Mask.Print(con); con.Printf("\n");
        con.Printf("router: "); r.Router.Print(con); con.Printf("\n");
        con.Printf("dns:    "); r.Dns.Print(con); con.Printf("\n");
        con.Printf("lease:  %u seconds\n", r.LeaseTime);

        if (Parameters::GetInstance().IsDnsEnabled() && !r.Dns.IsZero() &&
            !DnsResolver::GetInstance().IsInitialized())
        {
            if (DnsResolver::GetInstance().Init(dev, r.Dns))
            {
                con.Printf("DNS resolver started, server: ");
                r.Dns.Print(con);
                con.Printf("\n");
            }
        }
    }
    else
    {
        con.Printf("DHCP timeout\n");
    }
}

static void CmdMount(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* fsType = Stdlib::NextToken(args, end);
    if (fsType == nullptr)
    {
        con.Printf("usage: mount ramfs <path>\n");
        con.Printf("       mount nanofs <disk> <path>\n");
        con.Printf("       mount ext2 <disk> <path> [ro]\n");
        return;
    }
    char fsName[16];
    Stdlib::TokenCopy(fsType, end, fsName, sizeof(fsName));

    if (Stdlib::StrCmp(fsName, "ramfs") == 0)
    {
        const char* pathStart = Stdlib::NextToken(end, end);
        if (pathStart == nullptr)
        {
            con.Printf("usage: mount ramfs <path>\n");
            return;
        }
        char path[Vfs::MaxPath];
        Stdlib::TokenCopy(pathStart, end, path, sizeof(path));

        RamFs* fs = new (Mm::NoThrow) RamFs();
        if (fs == nullptr)
        {
            con.Printf("failed to allocate ramfs\n");
        }
        else if (!Vfs::GetInstance().Mount(path, fs))
        {
            delete fs;
            con.Printf("mount failed\n");
        }
        else
        {
            con.Printf("mounted ramfs on %s\n", path);
        }
    }
    else if (Stdlib::StrCmp(fsName, "nanofs") == 0)
    {
        const char* diskStart = Stdlib::NextToken(end, end);
        if (diskStart == nullptr)
        {
            con.Printf("usage: mount nanofs <disk> <path>\n");
            return;
        }
        char diskName[16];
        Stdlib::TokenCopy(diskStart, end, diskName, sizeof(diskName));

        const char* pathStart = Stdlib::NextToken(end, end);
        if (pathStart == nullptr)
        {
            con.Printf("usage: mount nanofs <disk> <path>\n");
            return;
        }
        char path[Vfs::MaxPath];
        Stdlib::TokenCopy(pathStart, end, path, sizeof(path));

        BlockDevice* dev = BlockDeviceTable::GetInstance().Find(diskName);
        if (dev == nullptr)
        {
            con.Printf("disk '%s' not found\n", diskName);
            return;
        }

        NanoFs* fs = new (Mm::NoThrow) NanoFs(dev);
        if (fs == nullptr)
        {
            con.Printf("failed to allocate nanofs\n");
            return;
        }

        if (!Vfs::GetInstance().Mount(path, fs))
        {
            delete fs;
            con.Printf("mount failed\n");
        }
        else
        {
            con.Printf("mounted nanofs on %s\n", path);
        }
    }
    else if (Stdlib::StrCmp(fsName, "ext2") == 0)
    {
        const char* diskStart = Stdlib::NextToken(end, end);
        if (diskStart == nullptr)
        {
            con.Printf("usage: mount ext2 <disk> <path> [ro]\n");
            return;
        }
        char diskName[16];
        Stdlib::TokenCopy(diskStart, end, diskName, sizeof(diskName));

        const char* pathStart = Stdlib::NextToken(end, end);
        if (pathStart == nullptr)
        {
            con.Printf("usage: mount ext2 <disk> <path> [ro]\n");
            return;
        }
        char path[Vfs::MaxPath];
        Stdlib::TokenCopy(pathStart, end, path, sizeof(path));

        bool readOnly = false;
        const char* optStart = Stdlib::NextToken(end, end);
        if (optStart != nullptr)
        {
            char opt[8];
            Stdlib::TokenCopy(optStart, end, opt, sizeof(opt));
            if (Stdlib::StrCmp(opt, "ro") != 0)
            {
                con.Printf("usage: mount ext2 <disk> <path> [ro]\n");
                return;
            }
            readOnly = true;
        }

        BlockDevice* dev = BlockDeviceTable::GetInstance().Find(diskName);
        if (dev == nullptr)
        {
            con.Printf("disk '%s' not found\n", diskName);
            return;
        }

        Ext2Fs* fs = new (Mm::NoThrow) Ext2Fs(dev);
        if (fs == nullptr)
        {
            con.Printf("failed to allocate ext2\n");
            return;
        }

        if (!Vfs::GetInstance().Mount(path, fs, readOnly))
        {
            delete fs;
            con.Printf("mount failed\n");
        }
        else
        {
            con.Printf("mounted ext2 on %s (%s)\n", path, fs->ReadOnly ? "ro" : "rw");
        }
    }
    else
    {
        con.Printf("unknown filesystem '%s'\n", fsName);
    }
}

static void CmdUmount(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* pathStart = Stdlib::NextToken(args, end);
    if (pathStart == nullptr)
    {
        con.Printf("usage: umount <path>\n");
        return;
    }
    char path[Vfs::MaxPath];
    Stdlib::TokenCopy(pathStart, end, path, sizeof(path));
    FileSystem* fs = Vfs::GetInstance().Unmount(path);
    if (fs == nullptr)
    {
        con.Printf("not mounted\n");
    }
    else
    {
        delete fs;
        con.Printf("unmounted %s\n", path);
    }
}

static void CmdMounts(const char* args, Stdlib::Printer& con)
{
    (void)args;
    Vfs::GetInstance().DumpMounts(con);
}

static void CmdLs(const char* args, Stdlib::Printer& con)
{
    char path[Vfs::MaxPath];
    const char* end;
    const char* pathStart = Stdlib::NextToken(args, end);
    if (pathStart == nullptr)
        Stdlib::StrnCpy(path, "/", sizeof(path));
    else
        Stdlib::TokenCopy(pathStart, end, path, sizeof(path));
    Vfs::GetInstance().ListDir(path, con);
}

static void CmdCat(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* pathStart = Stdlib::NextToken(args, end);
    if (pathStart == nullptr)
    {
        con.Printf("usage: cat <path>\n");
        return;
    }
    char path[Vfs::MaxPath];
    Stdlib::TokenCopy(pathStart, end, path, sizeof(path));
    Vfs::GetInstance().ReadFile(path, con);
}

static void CmdWrite(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* pathStart = Stdlib::NextToken(args, end);
    if (pathStart == nullptr)
    {
        con.Printf("usage: write <path> <text>\n");
        return;
    }
    char path[Vfs::MaxPath];
    Stdlib::TokenCopy(pathStart, end, path, sizeof(path));

    // Rest of the line after path is the content
    const char* content = end;
    while (*content == ' ')
        content++;

    ulong len = Stdlib::StrLen(content);
    if (Vfs::GetInstance().WriteFile(path, content, len))
    {
        con.Printf("wrote %u bytes\n", len);
    }
    else
    {
        con.Printf("write failed\n");
    }
}

static void CmdMkdir(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* pathStart = Stdlib::NextToken(args, end);
    if (pathStart == nullptr)
    {
        con.Printf("usage: mkdir <path>\n");
        return;
    }
    char path[Vfs::MaxPath];
    Stdlib::TokenCopy(pathStart, end, path, sizeof(path));
    if (Vfs::GetInstance().CreateDir(path))
    {
        con.Printf("created %s\n", path);
    }
    else
    {
        con.Printf("mkdir failed\n");
    }
}

static void CmdTouch(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* pathStart = Stdlib::NextToken(args, end);
    if (pathStart == nullptr)
    {
        con.Printf("usage: touch <path>\n");
        return;
    }
    char path[Vfs::MaxPath];
    Stdlib::TokenCopy(pathStart, end, path, sizeof(path));
    if (Vfs::GetInstance().CreateFile(path))
    {
        con.Printf("created %s\n", path);
    }
    else
    {
        con.Printf("touch failed\n");
    }
}

/* The name after the last slash: what a copy into a directory is called */
static const char* BaseName(const char* path)
{
    const char* base = path;
    for (const char* p = path; *p != '\0'; p++)
    {
        if (*p == '/' && p[1] != '\0')
            base = p + 1;
    }
    return base;
}

/* dst, or dst/<basename of src> when dst is an existing directory */
static bool ResolveCopyTarget(const char* src, const char* dst, char* out, ulong outSize)
{
    FileStat st;
    if (Vfs::GetInstance().Stat(dst, st) && st.Type == VNode::TypeDir)
    {
        ulong dstLen = Stdlib::StrLen(dst);
        bool slash = (dstLen > 0 && dst[dstLen - 1] == '/');
        int n = Stdlib::SnPrintf(out, outSize, slash ? "%s%s" : "%s/%s", dst, BaseName(src));
        return n > 0 && (ulong)n < outSize;
    }

    Stdlib::StrnCpy(out, dst, outSize);
    return Stdlib::StrLen(dst) < outSize;
}

static const ulong CopyChunk = 64 * 1024;

/* One file, through the file API in 64 KiB pieces: neither side has to
   fit in memory. Fails with the target left as it was written so far. */
static bool CopyFile(const char* src, const char* dst, Stdlib::Printer& con, ulong& copied)
{
    auto& vfs = Vfs::GetInstance();
    copied = 0;

    File* in = vfs.Open(src, Vfs::OpenRead);
    if (in == nullptr)
    {
        con.Printf("cp: cannot open %s\n", src);
        return false;
    }

    File* out = vfs.Open(dst, Vfs::OpenWrite | Vfs::OpenCreate | Vfs::OpenTruncate);
    if (out == nullptr)
    {
        con.Printf("cp: cannot create %s\n", dst);
        vfs.Close(in);
        return false;
    }

    u8* buf = (u8*)Mm::Alloc(CopyChunk, 0);
    if (buf == nullptr)
    {
        con.Printf("cp: alloc failed\n");
        vfs.Close(out);
        vfs.Close(in);
        return false;
    }

    bool ok = true;
    for (;;)
    {
        ulong got = 0;
        if (!vfs.Read(in, buf, CopyChunk, got))
        {
            con.Printf("cp: read from %s failed\n", src);
            ok = false;
            break;
        }
        if (got == 0)
            break;
        if (!vfs.Write(out, buf, got))
        {
            con.Printf("cp: write to %s failed\n", dst);
            ok = false;
            break;
        }
        copied += got;
    }

    Mm::Free(buf);
    vfs.Close(out);
    vfs.Close(in);
    return ok;
}

/* Deep enough for anything a rootfs holds; the paths cap it anyway */
static const ulong CopyMaxDepth = 32;

static bool CopyTree(const char* src, const char* dst, ulong depth, Stdlib::Printer& con,
                     ulong& files, ulong& bytes)
{
    auto& vfs = Vfs::GetInstance();

    if (depth >= CopyMaxDepth)
    {
        con.Printf("cp: %s: too deep\n", src);
        return false;
    }

    FileStat st;
    if (!vfs.Stat(dst, st))
    {
        if (!vfs.CreateDir(dst))
        {
            con.Printf("cp: cannot create directory %s\n", dst);
            return false;
        }
    }
    else if (st.Type != VNode::TypeDir)
    {
        con.Printf("cp: %s exists and is not a directory\n", dst);
        return false;
    }

    DirEntry entry;
    for (ulong i = 0; vfs.ReadDir(src, i, entry); i++)
    {
        char from[Vfs::MaxPath];
        char to[Vfs::MaxPath];
        ulong srcLen = Stdlib::StrLen(src);
        ulong dstLen = Stdlib::StrLen(dst);
        bool srcSlash = (srcLen > 0 && src[srcLen - 1] == '/');
        bool dstSlash = (dstLen > 0 && dst[dstLen - 1] == '/');
        int n1 = Stdlib::SnPrintf(from, sizeof(from), srcSlash ? "%s%s" : "%s/%s", src, entry.Name);
        int n2 = Stdlib::SnPrintf(to, sizeof(to), dstSlash ? "%s%s" : "%s/%s", dst, entry.Name);
        if (n1 <= 0 || (ulong)n1 >= sizeof(from) || n2 <= 0 || (ulong)n2 >= sizeof(to))
        {
            con.Printf("cp: path too long under %s\n", src);
            return false;
        }

        if (entry.Type == VNode::TypeDir)
        {
            if (!CopyTree(from, to, depth + 1, con, files, bytes))
                return false;
        }
        else
        {
            ulong copied = 0;
            if (!CopyFile(from, to, con, copied))
                return false;
            files++;
            bytes += copied;
        }
    }

    return true;
}

/* True when path is inside dir (or is dir itself) */
static bool IsUnder(const char* dir, const char* path)
{
    ulong dirLen = Stdlib::StrLen(dir);
    while (dirLen > 1 && dir[dirLen - 1] == '/')
        dirLen--;
    if (Stdlib::StrnCmp(path, dir, dirLen) != 0)
        return false;
    return dirLen == 1 || path[dirLen] == '\0' || path[dirLen] == '/';
}

static void CmdCp(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* tok = Stdlib::NextToken(args, end);
    bool recursive = false;
    if (tok != nullptr && Stdlib::StrnCmp(tok, "-r", 2) == 0 && (end - tok) == 2)
    {
        recursive = true;
        tok = Stdlib::NextToken(end, end);
    }
    if (tok == nullptr)
    {
        con.Printf("usage: cp [-r] <src> <dst>\n");
        return;
    }
    char src[Vfs::MaxPath];
    Stdlib::TokenCopy(tok, end, src, sizeof(src));

    tok = Stdlib::NextToken(end, end);
    if (tok == nullptr)
    {
        con.Printf("usage: cp [-r] <src> <dst>\n");
        return;
    }
    char dstArg[Vfs::MaxPath];
    Stdlib::TokenCopy(tok, end, dstArg, sizeof(dstArg));

    auto& vfs = Vfs::GetInstance();
    FileStat st;
    if (!vfs.Stat(src, st))
    {
        con.Printf("cp: %s not found\n", src);
        return;
    }

    char dst[Vfs::MaxPath];
    if (!ResolveCopyTarget(src, dstArg, dst, sizeof(dst)))
    {
        con.Printf("cp: path too long\n");
        return;
    }

    if (Stdlib::StrCmp(src, dst) == 0)
    {
        con.Printf("cp: %s and %s are the same file\n", src, dst);
        return;
    }

    if (st.Type == VNode::TypeDir)
    {
        if (!recursive)
        {
            con.Printf("cp: %s is a directory (use -r)\n", src);
            return;
        }
        if (IsUnder(src, dst))
        {
            con.Printf("cp: cannot copy %s into itself\n", src);
            return;
        }
        ulong files = 0;
        ulong bytes = 0;
        if (CopyTree(src, dst, 0, con, files, bytes))
            con.Printf("copied %u files, %u bytes to %s\n", files, bytes, dst);
        return;
    }

    ulong copied = 0;
    if (CopyFile(src, dst, con, copied))
        con.Printf("copied %u bytes to %s\n", copied, dst);
}

static void CmdDel(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* pathStart = Stdlib::NextToken(args, end);
    if (pathStart == nullptr)
    {
        con.Printf("usage: del <path>\n");
        return;
    }
    char path[Vfs::MaxPath];
    Stdlib::TokenCopy(pathStart, end, path, sizeof(path));
    if (Vfs::GetInstance().Remove(path))
    {
        con.Printf("removed %s\n", path);
    }
    else
    {
        con.Printf("del failed\n");
    }
}

static void CmdAppend(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* pathStart = Stdlib::NextToken(args, end);
    if (pathStart == nullptr)
    {
        con.Printf("usage: append <path> <text>\n");
        return;
    }
    char path[Vfs::MaxPath];
    Stdlib::TokenCopy(pathStart, end, path, sizeof(path));

    // Rest of the line after path is the content
    const char* content = end;
    while (*content == ' ')
        content++;

    ulong len = Stdlib::StrLen(content);
    auto& vfs = Vfs::GetInstance();
    File* file = vfs.Open(path, Vfs::OpenAppend | Vfs::OpenCreate);
    if (file == nullptr)
    {
        con.Printf("open failed\n");
        return;
    }

    if (vfs.Write(file, content, len))
        con.Printf("appended %u bytes\n", len);
    else
        con.Printf("write failed\n");
    vfs.Close(file);
}

static void CmdMv(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* oldStart = Stdlib::NextToken(args, end);
    if (oldStart == nullptr)
    {
        con.Printf("usage: mv <old> <new>\n");
        return;
    }
    char oldPath[Vfs::MaxPath];
    Stdlib::TokenCopy(oldStart, end, oldPath, sizeof(oldPath));

    const char* newStart = Stdlib::NextToken(end, end);
    if (newStart == nullptr)
    {
        con.Printf("usage: mv <old> <new>\n");
        return;
    }
    char newPath[Vfs::MaxPath];
    Stdlib::TokenCopy(newStart, end, newPath, sizeof(newPath));

    if (Vfs::GetInstance().Rename(oldPath, newPath))
        con.Printf("moved %s to %s\n", oldPath, newPath);
    else
        con.Printf("mv failed\n");
}

static void CmdStat(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* pathStart = Stdlib::NextToken(args, end);
    if (pathStart == nullptr)
    {
        con.Printf("usage: stat <path>\n");
        return;
    }
    char path[Vfs::MaxPath];
    Stdlib::TokenCopy(pathStart, end, path, sizeof(path));

    FileStat st;
    if (!Vfs::GetInstance().Stat(path, st))
    {
        con.Printf("not found\n");
        return;
    }

    if (st.Type == VNode::TypeDir)
        con.Printf("%s: directory, inode %u\n", path, st.Ino);
    else
        con.Printf("%s: file, %u bytes, inode %u\n", path, st.Size, st.Ino);
}

static void CmdSync(const char* args, Stdlib::Printer& con)
{
    (void)args;
    if (Vfs::GetInstance().Sync())
        con.Printf("synced\n");
    else
        con.Printf("sync failed\n");
}

/* fstest [dir] [size]: the filesystem self-test in dir (default /) with a
   big file of size bytes (default 300 KiB; a K or M suffix is taken) */
static void CmdFstest(const char* args, Stdlib::Printer& con)
{
    static const ulong DefaultBigSize = 300 * 1024;
    static const ulong MaxBigSize = 1024UL * 1024 * 1024;

    char dir[Vfs::MaxPath];
    Stdlib::StrnCpy(dir, "/", sizeof(dir));
    ulong bigSize = DefaultBigSize;

    const char* end;
    const char* dirStart = Stdlib::NextToken(args, end);
    if (dirStart != nullptr)
    {
        Stdlib::TokenCopy(dirStart, end, dir, sizeof(dir));

        const char* sizeStart = Stdlib::NextToken(end, end);
        if (sizeStart != nullptr)
        {
            char sizeText[24];
            ulong len = Stdlib::TokenCopy(sizeStart, end, sizeText, sizeof(sizeText));
            ulong mult = 1;
            if (len > 0 && (sizeText[len - 1] == 'K' || sizeText[len - 1] == 'k'))
            {
                mult = 1024;
                sizeText[len - 1] = '\0';
            }
            else if (len > 0 && (sizeText[len - 1] == 'M' || sizeText[len - 1] == 'm'))
            {
                mult = 1024 * 1024;
                sizeText[len - 1] = '\0';
            }
            if (!Stdlib::ParseUlong(sizeText, bigSize) || bigSize * mult > MaxBigSize)
            {
                con.Printf("usage: fstest [dir] [size[K|M]]\n");
                return;
            }
            bigSize *= mult;
        }
    }

    if (FsSelfTest(dir, bigSize, &con))
        con.Printf("fstest: passed (%s, %u byte file)\n", dir, bigSize);
    else
        con.Printf("fstest: FAILED\n");
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
    char path[Vfs::MaxPath];
    Stdlib::TokenCopy(pathStart, end, path, sizeof(path));

    auto& vfs = Vfs::GetInstance();
    File* file = vfs.Open(path, Vfs::OpenRead);
    if (file == nullptr)
    {
        con.Printf("open failed\n");
        return;
    }

    u8* buf = (u8*)Mm::Alloc(ChunkSize, 0);
    if (buf == nullptr)
    {
        con.Printf("alloc failed\n");
        vfs.Close(file);
        return;
    }

    u32 crc = 0;
    ulong total = 0;
    bool ok = true;
    for (;;)
    {
        ulong got = 0;
        if (!vfs.Read(file, buf, ChunkSize, got))
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
    vfs.Close(file);

    if (ok)
        con.Printf("%s: crc32 0x%p, %u bytes\n", path, (ulong)crc, total);
    else
        con.Printf("read failed\n");
}

static void PrintHex(Stdlib::Printer& con, const u8* buf, ulong len)
{
    static const char hex[] = "0123456789abcdef";
    for (ulong i = 0; i < len; i++)
    {
        char s[3];
        s[0] = hex[(buf[i] >> 4) & 0xF];
        s[1] = hex[buf[i] & 0xF];
        s[2] = '\0';
        con.PrintString(s);
    }
}

static void CmdSha256(const char* args, Stdlib::Printer& con)
{
    static const ulong ChunkSize = 64 * 1024;

    const char* end;
    const char* pathStart = Stdlib::NextToken(args, end);
    if (pathStart == nullptr)
    {
        con.Printf("usage: sha256 <path>\n");
        return;
    }
    char path[Vfs::MaxPath];
    Stdlib::TokenCopy(pathStart, end, path, sizeof(path));

    auto& vfs = Vfs::GetInstance();
    File* file = vfs.Open(path, Vfs::OpenRead);
    if (file == nullptr)
    {
        con.Printf("open failed\n");
        return;
    }

    u8* buf = (u8*)Mm::Alloc(ChunkSize, 0);
    if (buf == nullptr)
    {
        con.Printf("alloc failed\n");
        vfs.Close(file);
        return;
    }

    Sha256Hash hash;
    bool ok = true;
    for (;;)
    {
        ulong got = 0;
        if (!vfs.Read(file, buf, ChunkSize, got))
        {
            ok = false;
            break;
        }
        if (got == 0)
            break;
        hash.Update(buf, got);
    }

    Mm::Free(buf);
    vfs.Close(file);

    if (!ok)
    {
        con.Printf("read failed\n");
        return;
    }

    /* As sha256sum prints it, so a line of a release's SHA256SUMS compares
       by eye */
    u8 digest[Sha256Hash::DigestSize];
    if (!hash.Finish(digest))
    {
        con.Printf("hash failed\n");
        return;
    }
    PrintHex(con, digest, sizeof(digest));
    con.Printf("  %s\n", path);
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
    auto& vfs = Vfs::GetInstance();
    File* file = vfs.Open(path, Vfs::OpenRead);
    if (file == nullptr)
    {
        con.Printf("open failed\n");
        return nullptr;
    }

    size = vfs.GetSize(file);
    if (size < Stdlib::GrubEnvBlock::MinSize || size > GrubenvMaxSize)
    {
        con.Printf("%s: %u bytes is not a GRUB environment block\n", path, size);
        vfs.Close(file);
        return nullptr;
    }

    char* block = (char*)Mm::Alloc(size, 0);
    if (block == nullptr)
    {
        con.Printf("alloc failed\n");
        vfs.Close(file);
        return nullptr;
    }

    ulong total = 0;
    while (total < size)
    {
        ulong got = 0;
        if (!vfs.Read(file, block + total, size - total, got) || got == 0)
            break;
        total += got;
    }
    vfs.Close(file);

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
    char path[Vfs::MaxPath];
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
            con.Printf("cannot set %s: not a name GRUB takes, or no room in %u bytes\n",
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
    auto& vfs = Vfs::GetInstance();
    File* file = vfs.Open(path, Vfs::OpenWrite);
    bool ok = (file != nullptr) && vfs.Write(file, block, size);
    if (file != nullptr)
        vfs.Close(file);
    Mm::Free(block);

    if (!ok)
    {
        con.Printf("%s: write failed\n", path);
        return;
    }
    if (!vfs.Sync())
        con.Printf("sync failed\n");
}

/* What format's claim on its device says to whoever is refused it */
static const char FormatHolder[] = "format";

static void CmdFormat(const char* args, Stdlib::Printer& con)
{
    const char* end;
    const char* fsType = Stdlib::NextToken(args, end);
    if (fsType == nullptr)
    {
        con.Printf("usage: format nanofs <disk>\n");
        return;
    }
    char fsName[16];
    Stdlib::TokenCopy(fsType, end, fsName, sizeof(fsName));

    if (Stdlib::StrCmp(fsName, "nanofs") != 0)
    {
        con.Printf("unknown filesystem '%s'\n", fsName);
        return;
    }

    const char* diskStart = Stdlib::NextToken(end, end);
    if (diskStart == nullptr)
    {
        con.Printf("usage: format nanofs <disk>\n");
        return;
    }
    char diskName[16];
    Stdlib::TokenCopy(diskStart, end, diskName, sizeof(diskName));

    BlockDevice* dev = BlockDeviceTable::GetInstance().Find(diskName);
    if (dev == nullptr)
    {
        con.Printf("disk '%s' not found\n", diskName);
        return;
    }

    /* Not under a mounted filesystem, the disk log or a write test, nor over
       a disk one of those holds a partition of */
    auto& table = BlockDeviceTable::GetInstance();
    const char* heldBy = nullptr;
    const ulong claim = table.Claim(dev, FormatHolder, heldBy);
    if (claim == 0)
    {
        con.Printf("disk '%s' is in use by %s\n", diskName, heldBy);
        return;
    }

    bool formatted = false;
    {
        NanoFs fs(dev);
        formatted = fs.Format(dev);
    }
    table.Release(claim);

    if (formatted)
    {
        con.Printf("formatted %s as nanofs\n", diskName);
    }
    else
    {
        con.Printf("format failed\n");
    }
}

static void CmdVersion(const char* args, Stdlib::Printer& con)
{
    (void)args;
    con.Printf("nos %s (%s)\n", KERNEL_VERSION, KERNEL_GIT_REV);
}

static void CmdRandom(const char* args, Stdlib::Printer& con)
{
    ulong len = 16;
    if (args[0] != '\0')
    {
        if (!Stdlib::ParseUlong(args, len) || len == 0 || len > 1024)
        {
            con.Printf("usage: random [len] (1..1024, default 16)\n");
            return;
        }
    }

    auto& random = Random::GetInstance();
    if (!random.IsSeeded())
    {
        con.Printf("entropy pool is not seeded\n");
        return;
    }

    u8 buf[1024];
    random.GetBytes(buf, len);

    static const char hex[] = "0123456789abcdef";
    for (ulong i = 0; i < len; i++)
    {
        char s[3];
        s[0] = hex[(buf[i] >> 4) & 0xF];
        s[1] = hex[buf[i] & 0xF];
        s[2] = '\0';
        con.PrintString(s);
    }
    con.Printf("\n");
}

static void CmdEntropy(const char* args, Stdlib::Printer& con)
{
    auto& random = Random::GetInstance();

    if (Stdlib::StrCmp(args, "reseed") == 0)
    {
        /* Worth having by hand: a source can appear after the pool was seeded
           (a virtio-rng behind a bus that was scanned late), and on a machine
           whose only console is a UDP socket this is how one finds out
           whether it answers. */
        random.Reseed();
    }
    else if (args[0] != '\0')
    {
        con.Printf("usage: entropy [reseed]\n");
        return;
    }

    random.Dump(con);
}

static void DumpStackTrace(ulong* frames, size_t count, Stdlib::Printer& con)
{
    auto& symtab = SymbolTable::GetInstance();
    for (size_t i = 0; i < count; i++)
    {
        char where[SymbolTable::DescribeMax];
        if (symtab.Describe(frames[i], where, sizeof(where)))
            con.Printf("  [%u] 0x%p %s\n", (ulong)i, frames[i], where);
        else
            con.Printf("  [%u] 0x%p\n", (ulong)i, frames[i]);
    }
}

struct BtCtx
{
    ulong Frames[16];
    size_t Count;
};

static void CmdBtIPIFunc(void* ctx, Context* ipiCtx)
{
    auto* bt = static_cast<BtCtx*>(ctx);
    bt->Count = StackTrace::CaptureFrom(ipiCtx->GetFramePointer(), bt->Frames, Stdlib::ArraySize(bt->Frames));
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
        con.Printf("task %u not found\n", pid);
        return;
    }

    ulong frames[16];
    size_t count = 0;

    Task* self = Task::GetCurrentTask();
    if (task.Get() == self)
    {
        /* Target is the current task on this CPU */
        count = StackTrace::Capture(frames, Stdlib::ArraySize(frames));
        con.Printf("task %u (%s) running on current cpu:\n", task->Pid, task->GetName());
        DumpStackTrace(frames, count, con);
        return;
    }

    long state = task->State.Get();
    if (state == Task::StateRunning)
    {
        /* Task is running on another CPU — find which one */
        ulong cpuMask = CpuTable::GetInstance().GetRunningCpus();
        ulong runCpu = ~0UL;
        for (ulong i = 0; i < MaxCpus; i++)
        {
            if (!(cpuMask & (1UL << i)))
                continue;
            auto& cpuTaskQueue = CpuTable::GetInstance().GetCpu(i).GetTaskQueue();
            if (task->TaskQueue == &cpuTaskQueue)
            {
                runCpu = i;
                break;
            }
        }

        if (runCpu == ~0UL)
        {
            con.Printf("task %u (%s) running but cpu not found\n",
                task->Pid, task->GetName());
            return;
        }

        BtCtx btCtx;
        btCtx.Count = 0;

        IPITask ipiTask(CmdBtIPIFunc, &btCtx);
        CpuTable::GetInstance().GetCpu(runCpu).QueueIPITask(ipiTask);

        con.Printf("task %u (%s) running on cpu %u:\n",
            task->Pid, task->GetName(), runCpu);
        DumpStackTrace(btCtx.Frames, btCtx.Count, con);
        return;
    }

    if (state == Task::StateExited)
    {
        con.Printf("task %u (%s) has exited\n", task->Pid, task->GetName());
        return;
    }

    /* Task is waiting/sleeping — walk from saved context */
    ulong savedRsp = task->Rsp;
    if (savedRsp == 0)
    {
        con.Printf("task %u (%s) has no saved context\n", task->Pid, task->GetName());
        return;
    }

    count = StackTrace::CaptureFrom(Hal::TaskSavedFramePointer(savedRsp),
        frames, Stdlib::ArraySize(frames));

    con.Printf("task %u (%s) state %u:\n", task->Pid, task->GetName(), (ulong)state);
    DumpStackTrace(frames, count, con);
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
    else
    {
        con.Printf("usage: panic [pf|div0|ud]\n");
        con.Printf("  (no arg) - direct panic\n");
        con.Printf("  pf       - page fault (null deref)\n");
        con.Printf("  div0     - divide by zero\n");
        con.Printf("  ud       - invalid opcode\n");
    }
}

/* Each in a task of its own, which the shell waits for only so long: an
   unload waits out a module command still running, however long it runs */
static void CmdInsmod(const char* args, Stdlib::Printer& con)
{
    if (args[0] == '\0')
    {
        con.Printf("usage: insmod <path>\n");
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
/* The most of a script read, and the longest line run */
static const ulong ScriptSizeMax = 16 * Const::KB;
static const ulong ScriptLineMax = 255;
static const ulong ScriptTag = 'Rc  ';

/* A script, NUL-terminated, in a buffer from Mm::Alloc the caller frees --
   or nullptr, said on out, when it cannot be read. Found where Vfs::Locate
   finds it. Past ScriptSizeMax only its whole lines are taken, whole comes
   back false, and it is said: a command cut at the limit would run as some
   other command. */
static char* ReadScript(const char* path, ulong& size, bool& whole, Stdlib::Printer& out)
{
    auto& vfs = Vfs::GetInstance();
    char at[Vfs::MaxPath];
    File* file = vfs.Locate(path, at, sizeof(at)) ? vfs.Open(at, Vfs::OpenRead) : nullptr;
    if (file == nullptr)
    {
        out.Printf("rc: cannot open %s\n", path);
        return nullptr;
    }

    char* text = static_cast<char*>(Mm::Alloc(ScriptSizeMax + 1, ScriptTag));
    if (text == nullptr)
    {
        vfs.Close(file);
        out.Printf("rc: no memory to read %s\n", path);
        return nullptr;
    }

    size = 0;
    bool ok = true;
    while (size < ScriptSizeMax)
    {
        ulong got = 0;
        if (!vfs.Read(file, text + size, ScriptSizeMax - size, got))
        {
            ok = false;
            break;
        }
        if (got == 0)
            break;
        size += got;
    }
    ulong total = vfs.GetSize(file);
    vfs.Close(file);

    if (!ok)
    {
        Mm::Free(text);
        out.Printf("rc: cannot read %s\n", path);
        return nullptr;
    }
    whole = (total <= size);
    if (!whole)
    {
        ulong keep = size;
        while (keep > 0 && text[keep - 1] != '\n')
            keep--;
        out.Printf("rc: %s is %u bytes; only its whole lines in the first %u are read\n",
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

    virtual void Printf(const char *fmt, ...) override
    {
        va_list args;
        va_start(args, fmt);
        VPrintf(fmt, args);
        va_end(args);
    }

    virtual void VPrintf(const char *fmt, va_list args) override
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
            Trace(0, "%s[%u more bytes it printed not logged]", Prefix, Dropped);

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
    char at[Vfs::MaxPath];
    if (!Vfs::GetInstance().Locate(RcPath, at, sizeof(at)))
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
        con.Printf("%u  %s\n", number, shown);
    }
    if (number == 0)
        con.Printf("rc: %s is empty\n", RcPath);

    Mm::Free(text);
}

/* Writes a new /etc/rc: the lines of the old one but the one numbered skip
   (0: none), then add if there is one -- through Vfs::ReplaceFile, since
   what it is for is the next boot, and a full disk must not leave it empty.
   One edit at a time (RcLock): two at once would each write over the other
   one's line. */
static bool RcRewrite(ulong skip, const char* add, Stdlib::Printer& con)
{
    Stdlib::AutoLock lock(Cmd::GetInstance().GetRcLock());

    auto& vfs = Vfs::GetInstance();
    FileStat st;
    char at[Vfs::MaxPath];

    ulong size = 0;
    char* old = nullptr;
    if (vfs.Locate(RcPath, at, sizeof(at)))
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
    else if (!vfs.Stat(RcDir, st) && !vfs.CreateDir(RcDir))
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
            con.Printf("rc: %s has no line %u\n", RcPath, skip);
            return false;
        }
    }

    ulong addLen = (add != nullptr) ? Stdlib::StrLen(add) : 0;
    if (size + addLen + 1 > ScriptSizeMax)
    {
        if (old != nullptr)
            Mm::Free(old);
        con.Printf("rc: %s would pass %u bytes\n", RcPath, ScriptSizeMax);
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

    bool ok = vfs.ReplaceFile(RcPath, text, pos);
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
            con.Printf("rc: at most %u characters a line\n", ScriptLineMax);
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
        auto& vfs = Vfs::GetInstance();
        char at[Vfs::MaxPath];
        if (!vfs.Locate(RcPath, at, sizeof(at)))
        {
            con.Printf("rc: no %s\n", RcPath);
        }
        else if (vfs.Remove(at))
        {
            /* And the other of the pair, should a cut-short edit have left both */
            if (vfs.Locate(RcPath, at, sizeof(at)))
                vfs.Remove(at);
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
    { "netload",   CmdNetload,   "netload [start [port] [sink]|stop|reset] - udp load target" },
    { "nicdump",   CmdNicdump,   "nicdump - r8125 chip and ring state" },
    { "igbdump",   CmdIgbdump,   "igbdump - igb chip and ring state" },
    { "disklog",   CmdDisklog,   "disklog - kernel log to disk area state" },
    { "top",       CmdTop,       "top [ms] - per-task cpu use over a sampling window" },
    { "profile",   CmdProfile,   "profile [ms] [pid] - sample where the kernel spends its time" },
    { "watchdog",  CmdWatchdog,  "watchdog - show watchdog stats" },
    { "memusage",  CmdMemusage,  "memusage - show memory usage stats" },
    { "meminfo",   CmdMeminfo,   "meminfo - show the firmware memory map and what of it is used" },
    { "memcheck",  CmdMemcheck,  "memcheck - verify no reserved page reached the free list" },
    { "irqstat",   CmdIrqstat,   "irqstat - show interrupt statistics" },
    { "pci",       CmdPci,       "pci - show pci devices" },
    { "usb",       CmdUsb,       "usb - show usb controllers and ports" },
    { "disks",     CmdDisks,     "disks - list block devices" },
    { "partitions", CmdPartitions, "partitions <disk> - show partition table" },
    { "diskread",  CmdDiskread,  "diskread <disk> <sector> - read sector" },
    { "diskwrite", CmdDiskwrite, "diskwrite <disk> <sector> <hex> - write sector" },
    { "net",       CmdNet,       "net - list network devices" },
    { "arp",       CmdArp,       "arp - show ARP table" },
    { "netpool",   CmdNetpool,   "netpool - show the recycled net frame pool" },
    { "netconsole", CmdNetconsole, "netconsole - show netconsole state" },
    { "icmpstat",  CmdIcmpstat,  "icmpstat - show ICMP statistics" },
    { "tcpstat",   CmdTcpstat,   "tcpstat - show TCP connections and statistics" },
    { "wget",      CmdWget,      "wget [-o <path>] <url> [path] - HTTP(S) GET, streamed to a file (up to 20 MB)" },
    { "udpsend",   CmdUdpsend,   "udpsend <ip> <port> <msg> - send UDP packet" },
    { "ping",      CmdPing,      "ping <ip|hostname> - send ICMP echo" },
    { "nslookup",  CmdNslookup,  "nslookup <hostname> - resolve hostname" },
    { "dnsflush",  CmdDnsflush,  "dnsflush - flush DNS cache" },
    { "dhcp",      CmdDhcp,      "dhcp [dev] - obtain IP via DHCP" },
    { "format",    CmdFormat,    "format nanofs <disk> - format disk" },
    { "mount",     CmdMount,     "mount <ramfs|nanofs|ext2> ... - mount filesystem" },
    { "umount",    CmdUmount,    "umount <path> - unmount filesystem" },
    { "mounts",    CmdMounts,    "mounts - list mount points" },
    { "ls",        CmdLs,        "ls [path] - list directory (default /)" },
    { "cat",       CmdCat,       "cat <path> - show file content" },
    { "write",     CmdWrite,     "write <path> <text> - write to file" },
    { "mkdir",     CmdMkdir,     "mkdir <path> - create directory" },
    { "touch",     CmdTouch,     "touch <path> - create empty file" },
    { "cp",        CmdCp,        "cp [-r] <src> <dst> - copy a file, or a directory tree with -r" },
    { "rm",        CmdDel,       "rm <path> - remove file or directory (recursively)" },
    { "del",       CmdDel,       nullptr },
    { "append",    CmdAppend,    "append <path> <text> - append text to file" },
    { "mv",        CmdMv,        "mv <old> <new> - rename or move a file or directory" },
    { "stat",      CmdStat,      "stat <path> - show type, size and inode" },
    { "sync",      CmdSync,      "sync - flush filesystems to disk" },
    { "fstest",    CmdFstest,    "fstest [dir] [size] - filesystem self-test" },
    { "crc32",     CmdCrc32,     "crc32 <path> - CRC-32 of a file" },
    { "sha256",    CmdSha256,    "sha256 <path> - SHA-256 of a file, as sha256sum prints it" },
    { "grubenv",   CmdGrubenv,   "grubenv <path> [name=value ...] - show or set GRUB environment variables" },
    { "insmod",    CmdInsmod,    "insmod <path> - load a kernel module (.ko)" },
    { "rmmod",     CmdRmmod,     "rmmod <name> - unload a kernel module" },
    { "lsmod",     CmdLsmod,     "lsmod - list the loaded kernel modules" },
    { "rc",        CmdRc,        "rc [add <command line>|del <n>|clear|run] - show or edit /etc/rc, run at boot" },
    { "random",    CmdRandom,    "random [len] - get random bytes as hex" },
    { "entropy",   CmdEntropy,   "entropy [reseed] - show the random pool and its sources" },
    { "version",   CmdVersion,   "version - show kernel version" },
    { "bt",        CmdBt,        "bt <pid> - show task backtrace" },
    { "panic",     CmdPanic,     "panic [pf|div0|ud] - trigger kernel panic" },
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
    CmdLine[0] = '\0';
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

void Cmd::ProcessCmd(const char *cmd)
{
    auto& con = Console::GetInstance();
    Dispatch(cmd, con);
    con.Printf("$");
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
                out.Printf("rc: line %u is longer than %u characters, skipped\n", number, ScriptLineMax);
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
    char at[Vfs::MaxPath];
    if (!Vfs::GetInstance().Locate(RcPath, at, sizeof(at)))
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
   -- and a task that sleeps yields rather than blocks, so on a CPU carrying
   the shell, DHCP and USB poll tasks that is never: the request would sit
   unseen for good. Let that task take its turn as an ordinary one from
   here on; it has nothing left to do but notice. */
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
    GetDhcpClient().Stop();
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
    size_t pos = 0;
    bool overflow = false;

    auto& con = Console::GetInstance();

    /* Wait for startup trace output to settle before suppressing console */
    Sleep(100 * Const::NanoSecsInMs);

    Tracer::GetInstance().SetConsoleSuppressed(true);

    ShowBanner(con);

    if (Parameters::GetInstance().IsDhcpAuto())
    {
        NetDevice* dev = NetDeviceTable::GetInstance().Find("eth0");
        if (dev)
        {
            con.Printf("DHCP auto on eth0...\n");
            if (GetDhcpClient().Start(dev))
            {
                for (ulong i = 0; i < 100 && !GetDhcpClient().IsReady(); i++)
                    Sleep(100 * Const::NanoSecsInMs);

                if (GetDhcpClient().IsReady())
                {
                    DhcpResult r = GetDhcpClient().GetResult();
                    con.Printf("DHCP ip: ");
                    r.Ip.Print(con);
                    con.Printf("\n");

                    if (Parameters::GetInstance().IsDnsEnabled() && !r.Dns.IsZero())
                    {
                        if (DnsResolver::GetInstance().Init(dev, r.Dns))
                        {
                            con.Printf("DNS resolver started, server: ");
                            r.Dns.Print(con);
                            con.Printf("\n");
                        }
                    }
                }
                else
                {
                    con.Printf("DHCP auto timeout\n");
                }
            }
            else
            {
                con.Printf("DHCP auto failed\n");
            }
        }
    }

    RunBootScript();

    con.Printf("$");

    while (!Task::GetCurrentTask()->IsStopping())
    {
        KeyEvent keyEvent = {};
        bool hasEvent = false;
        bool backspace = false;
        {
            Stdlib::AutoLock lock(Lock);
            if (!Buf.IsEmpty())
            {
                keyEvent = Buf.Get();
                hasEvent = true;
                backspace = (keyEvent.Code == 0xE) ? true : false;
            }
        }

        if (hasEvent)
        {
            if (backspace)
            {
                if (pos > 0)
                    con.Backspace();
            }
            else
            {
                con.Printf("%c", keyEvent.Char);
            }

            if (keyEvent.Char == '\n')
            {
                CmdLine[pos] = '\0';
                if (!overflow)
                {
                    ProcessCmd(CmdLine);
                }
                else
                {
                    con.Printf("command too large\n");
                    con.Printf("$");
                    overflow = false;
                }
                Stdlib::MemSet(CmdLine, 0, sizeof(CmdLine));
                pos = 0;
            }
            else
            {
                if (pos < (Stdlib::ArraySize(CmdLine) - 1))
                {
                    if (backspace)
                    {
                        if (pos > 0)
                        {
                            pos--;
                            CmdLine[pos] = '\0';
                        }
                    }
                    else
                    {
                        CmdLine[pos++] = keyEvent.Char;
                    }
                }
                else
                {
                    overflow = true;
                }
            }
        }

        Sleep(10 * Const::NanoSecsInMs);
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
    Stdlib::AutoLock lock(Lock);
    if (!Active)
        return;

    KeyEvent e;
    e.Char = c;
    e.Code = code;

    if (!Buf.Put(e))
    {
        Trace(0, "Can't save char");
        return;
    }
}

}
