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
#include "entropy.h"
#include "random.h"
#include "console.h"
#include "mutex.h"
#include "task.h"
#include "stack_probe.h"
#include "stack_trace.h"
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
    /* The window's count first: it is what the load being looked at did. The
       one since boot starts at one per CPU, on its idle task, that the boot
       self-test makes on purpose (TestPreemptDeferred). */
    con.Printf("preemptions deferred: %u in the window, %u of them on an idle task; "
        "%u since boot, %u on an idle task\n",
        (ulong)(deferred1 - deferred0), (ulong)(deferredIdle1 - deferredIdle0),
        (ulong)deferred1, (ulong)deferredIdle1);
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
        con.Printf("%s: crc32 0x%p, %u bytes\n", path, (ulong)crc, total);
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
        con.Printf("%s: %u bytes is not a GRUB environment block\n", path, size);
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
        con.Printf("%u  %s\n", number, shown);
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
    { "profile",   CmdProfile,   "profile [ms] [pid] - sample where the kernel spends its time" },
    { "watchdog",  CmdWatchdog,  "watchdog - show watchdog stats" },
    { "memusage",  CmdMemusage,  "memusage - show memory usage stats" },
    { "meminfo",   CmdMeminfo,   "meminfo - show the firmware memory map and what of it is used" },
    { "memcheck",  CmdMemcheck,  "memcheck - verify no reserved page reached the free list" },
    { "irqstat",   CmdIrqstat,   "irqstat - show interrupt statistics" },
    { "pci",       CmdPci,       "pci - show pci devices" },
    { "crc32",     CmdCrc32,     "crc32 <path> - CRC-32 of a file" },
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
    size_t pos = 0;
    bool overflow = false;

    auto& con = Console::GetInstance();

    /* Wait for startup trace output to settle before suppressing console */
    Sleep(100 * Const::NanoSecsInMs);

    Tracer::GetInstance().SetConsoleSuppressed(true);

    ShowBanner(con);

    if (Parameters::GetInstance().IsDhcpAuto())
        rust_net_dhcp_auto(&con);

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
