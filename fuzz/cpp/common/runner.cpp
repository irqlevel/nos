// The runner: random bytes from a seed, and whatever ends an input early
// caught -- a broken invariant, a kernel panic, an address or
// undefined-behaviour report from the sanitizers the target is built with, a
// crash, a hang -- each reported with the seed and iteration that make it
// again, and the input in a file to replay. The same runner as
// fuzz/common/runner.rs, for a target that is one binary, but for one thing:
// the inputs run in batches, one after another in a process forked for the
// batch, the target's Reset between them, where fs-fuzz forks for each. A
// fork of a process under the address sanitizer costs macOS fifteen
// milliseconds, a hundred inputs' worth. A batch ends at a finding, which is
// then run again alone in a process of its own -- the report is that run's --
// and the next batch starts after it.
#include "host.h"

#include "fuzz.h"
#include "kernel.h"

#include <errno.h>
#include <poll.h>
#include <signal.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include <map>
#include <set>
#include <string>
#include <vector>

/* A leak report at a replay's exit would be about the kernel's statics; an
   input's process ends by _exit, which reports nothing. */
extern "C" const char* __asan_default_options()
{
    return "detect_leaks=0:allocator_may_return_null=1:handle_abort=1";
}

extern "C" const char* __ubsan_default_options()
{
    return "print_stacktrace=1";
}

namespace Fuzz
{

namespace
{

bool Counting = false;
bool Trace = false;
std::set<std::string> ReachedStates;

/* What a batch's process sends up at its end: how many of its inputs
   reached each state. */
const char StatsMark[] = "\x01stats";

/* How long one input may run before it is a hang. */
const int HangSeconds = 20;

/* SplitMix64: the seed's stream of random words. */
struct Rng
{
    uint64_t State;

    uint64_t Next()
    {
        State += 0x9E3779B97F4A7C15ULL;
        uint64_t z = State;
        z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9ULL;
        z = (z ^ (z >> 27)) * 0x94D049BB133111EBULL;
        return z ^ (z >> 31);
    }
};

/* The input of iteration iter under seed: its length and bytes from a
   stream of their own, so any one iteration can be made again without the
   ones before it. */
std::vector<uint8_t> MakeInput(const char* target, uint64_t seed, uint64_t iter, size_t maxLen)
{
    uint64_t h = seed ^ 0xCBF29CE484222325ULL;
    for (const char* p = target; *p != '\0'; p++)
        h = (h ^ static_cast<uint8_t>(*p)) * 0x100000001B3ULL;
    Rng rng{h ^ (iter * 0x9E3779B97F4A7C15ULL)};
    size_t len;
    switch (rng.Next() % 4)
    {
    case 0:
        len = rng.Next() % std::max<uint64_t>(maxLen / 32, 64);
        break;
    case 1:
        len = rng.Next() % std::max<uint64_t>(maxLen / 4, 512);
        break;
    default:
        len = rng.Next() % maxLen;
        break;
    }
    std::vector<uint8_t> v;
    v.reserve(len + 8);
    while (v.size() < len)
    {
        uint64_t w = rng.Next();
        for (int i = 0; i < 8; i++)
            v.push_back(static_cast<uint8_t>(w >> (8 * i)));
    }
    v.resize(len);
    return v;
}

void WriteAll(int fd, const char* s, size_t n)
{
    while (n > 0)
    {
        ssize_t w = write(fd, s, n);
        if (w <= 0)
        {
            if (w < 0 && errno == EINTR)
                continue;
            return;
        }
        s += w;
        n -= static_cast<size_t>(w);
    }
}

std::string Hex(const std::vector<uint8_t>& data)
{
    static const char Digits[] = "0123456789abcdef";
    std::string s;
    for (uint8_t b : data)
    {
        s += Digits[b >> 4];
        s += Digits[b & 0xF];
    }
    return s;
}

bool Unhex(const std::string& text, std::vector<uint8_t>& out)
{
    std::string s;
    for (char c : text)
    {
        if (c != '\n' && c != '\r' && c != ' ')
            s += c;
    }
    if (s.size() % 2 != 0)
        return false;
    for (size_t i = 0; i < s.size(); i += 2)
    {
        char pair[3] = {s[i], s[i + 1], '\0'};
        char* end;
        long v = strtol(pair, &end, 16);
        if (*end != '\0')
            return false;
        out.push_back(static_cast<uint8_t>(v));
    }
    return true;
}

void RunInput(const std::vector<uint8_t>& data)
{
    Input input(data.data(), data.size());
    TheTarget.Run(input);
}

/* What became of an input. */
enum class Outcome
{
    Pass,
    Finding,
    Crash,
    Hang,
};

double Now()
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec + ts.tv_nsec / 1e9;
}

/* What a batch's process tells its parent without a word: the input it is
   on, since when, and how far it got. */
struct Shared
{
    volatile uint64_t Current;
    volatile uint64_t Done;
    volatile double Started;
};

Shared* Progress;

/* The parameters of a run, which a batch's process inherits. */
struct Run
{
    uint64_t Seed;
    uint64_t Gate;
    long Seconds;
    double Start;

    bool Over(uint64_t iter) const
    {
        return (Seconds >= 0) ? (Now() - Start >= Seconds) : (iter >= Gate);
    }
};

/* A process of its own for data, or for the batch from first on; its
   stderr, where the sanitizers report, is the pipe its parent reads. */
Outcome Supervise(pid_t pid, int fd, std::string& message, int& signal)
{
    bool hung = false;
    message.clear();
    for (;;)
    {
        struct pollfd pfd = {fd, POLLIN, 0};
        int ready = poll(&pfd, 1, 100);
        if (ready < 0 && errno != EINTR)
            break;
        if (ready > 0)
        {
            char buf[4096];
            ssize_t n = read(fd, buf, sizeof(buf));
            if (n <= 0)
                break;
            message.append(buf, static_cast<size_t>(n));
            /* The end is what says what happened. */
            if (message.size() > (1 << 20))
                message.erase(0, message.size() - (1 << 19));
            continue;
        }
        if (Now() - Progress->Started > HangSeconds)
        {
            hung = true;
            kill(pid, SIGKILL);
            break;
        }
    }
    int status = 0;
    while (waitpid(pid, &status, 0) < 0 && errno == EINTR)
    {
    }
    close(fd);
    if (hung)
        return Outcome::Hang;
    if (WIFSIGNALED(status))
    {
        signal = WTERMSIG(status);
        return Outcome::Crash;
    }
    return (WEXITSTATUS(status) != 0) ? Outcome::Finding : Outcome::Pass;
}

pid_t Spawn(int& fd)
{
    int fds[2];
    if (pipe(fds) != 0)
    {
        perror("pipe");
        exit(2);
    }
    fflush(stdout);
    fflush(stderr);
    pid_t pid = fork();
    if (pid < 0)
    {
        perror("fork");
        exit(2);
    }
    if (pid == 0)
    {
        close(fds[0]);
        /* The sanitizers report on stderr: that is the pipe now. */
        dup2(fds[1], STDERR_FILENO);
        close(fds[1]);
        return 0;
    }
    close(fds[1]);
    fd = fds[0];
    return pid;
}

/* The one input, alone in a fresh process: a finding's report. */
Outcome RunAlone(const std::vector<uint8_t>& data, std::string& message, int& signal)
{
    int fd;
    Progress->Started = Now();
    pid_t pid = Spawn(fd);
    if (pid == 0)
    {
        RunInput(data);
        _exit(0);
    }
    return Supervise(pid, fd, message, signal);
}

/* Inputs from first on, one after another, until the run is over or one of
   them ends the process; what each reached is counted into reached. */
Outcome RunBatch(const Run& run, uint64_t first, std::string& message, int& signal,
    std::map<std::string, uint64_t>& reached)
{
    int fd;
    Progress->Current = first;
    Progress->Done = first;
    Progress->Started = Now();
    pid_t pid = Spawn(fd);
    if (pid == 0)
    {
        std::map<std::string, uint64_t> counts;
        for (uint64_t i = first; !run.Over(i); i++)
        {
            std::vector<uint8_t> data = MakeInput(TheTarget.Name, run.Seed, i, TheTarget.MaxLen);
            Progress->Current = i;
            Progress->Started = Now();
            if (i != first)
            {
                ResetKernel();
                TheTarget.Reset();
            }
            RunInput(data);
            for (const std::string& st : ReachedStates)
                counts[st]++;
            ReachedStates.clear();
            Progress->Done = i + 1;
        }
        std::string up = StatsMark;
        for (const auto& c : counts)
            up += "\n" + std::to_string(c.second) + "\t" + c.first;
        WriteAll(STDERR_FILENO, up.data(), up.size());
        _exit(0);
    }
    Outcome outcome = Supervise(pid, fd, message, signal);
    if (outcome == Outcome::Pass)
    {
        size_t at = message.find(StatsMark);
        if (at != std::string::npos)
        {
            std::string rest = message.substr(at + sizeof(StatsMark) - 1);
            size_t pos = 0;
            while (pos < rest.size())
            {
                size_t nl = rest.find('\n', pos);
                std::string line = rest.substr(pos, (nl == std::string::npos) ? std::string::npos : nl - pos);
                size_t tab = line.find('\t');
                if (tab != std::string::npos)
                    reached[line.substr(tab + 1)] += strtoull(line.c_str(), nullptr, 10);
                if (nl == std::string::npos)
                    break;
                pos = nl + 1;
            }
            message.erase(at);
        }
    }
    return outcome;
}

std::string Trim(const std::string& s)
{
    size_t b = s.find_first_not_of(" \t\r\n");
    size_t e = s.find_last_not_of(" \t\r\n");
    return (b == std::string::npos) ? std::string() : s.substr(b, e - b + 1);
}

/* The line that says what happened, of a report that may have many. */
std::string Headline(const std::string& report)
{
    size_t at = 0;
    while (at < report.size())
    {
        size_t nl = report.find('\n', at);
        std::string line = report.substr(at, (nl == std::string::npos) ? std::string::npos : nl - at);
        if (line.find("FINDING: ") != std::string::npos || line.find("runtime error:") != std::string::npos ||
            line.find("SUMMARY: ") != std::string::npos)
            return Trim(line);
        if (nl == std::string::npos)
            break;
        at = nl + 1;
    }
    std::string first = report.substr(0, report.find('\n'));
    return Trim(first);
}

/* Where a finding is: the same check failing again is the same bug,
   whatever the names and numbers it says this time. */
std::string Place(const std::string& headline)
{
    size_t p = headline.find("FINDING: ");
    if (p != std::string::npos)
    {
        size_t at = headline.rfind(" at ");
        return (at != std::string::npos) ? headline.substr(at + 4) : headline;
    }
    p = headline.find(": runtime error:");
    if (p != std::string::npos)
    {
        /* file:line:col -- the column is the same check */
        std::string where = headline.substr(0, p);
        size_t colon = where.rfind(':');
        return (colon != std::string::npos) ? where.substr(0, colon) : where;
    }
    p = headline.find("SUMMARY: ");
    if (p != std::string::npos)
    {
        std::string s = headline.substr(p);
        size_t in = s.find(" in ");
        return (in != std::string::npos) ? s.substr(0, in) : s;
    }
    std::string s;
    for (char c : headline)
    {
        if (c < '0' || c > '9')
            s += c;
    }
    return s;
}

[[noreturn]] void Usage()
{
    fprintf(stderr, "usage: %s [--seed N] [--iterations N | --seconds S] [--keep-going]\n", TheTarget.Name);
    fprintf(stderr, "       (with neither, the target's own number of inputs: the gate)\n");
    fprintf(stderr, "       %s --replay HEXFILE [--trace]\n", TheTarget.Name);
    exit(2);
}

}

uint32_t Input::Value32()
{
    static const uint32_t Edges[] = {0, 1, 2, 3, 0x7F, 0x80, 0xFF, 0x100, 0xFFFF, 0x10000, 0x7FFFFFFF,
                                     0x80000000, 0xFFFFFFFE, 0xFFFFFFFF};
    switch (U8() % 4)
    {
    case 0:
        return Pick(Edges);
    case 1:
        return 1u << (U8() % 32);
    default:
        return U32();
    }
}

uint64_t Input::Value64()
{
    static const uint64_t Edges[] = {0, 1, 2, 0xFFFFFFFF, 0x100000000ULL, 0x7FFFFFFFFFFFFFFFULL,
                                     0x8000000000000000ULL, 0xFFFFFFFFFFFFF000ULL, 0xFFFFFFFFFFFFFFFFULL};
    switch (U8() % 4)
    {
    case 0:
        return Pick(Edges);
    case 1:
        return 1ULL << (U8() % 64);
    case 2:
        return Value32();
    default:
        return U64();
    }
}

std::vector<uint8_t> Noise(uint32_t seed, size_t n)
{
    uint64_t x = seed | (1ULL << 40);
    std::vector<uint8_t> v;
    v.reserve(n + 8);
    while (v.size() < n)
    {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        for (int i = 0; i < 8; i++)
            v.push_back(static_cast<uint8_t>(x >> (8 * i)));
    }
    v.resize(n);
    return v;
}

void Reached(const char* what)
{
    if (Counting)
        ReachedStates.insert(what);
}

bool Tracing()
{
    return Trace;
}

void Say(const char* fmt, ...)
{
    if (!Trace)
        return;
    va_list args;
    va_start(args, fmt);
    vfprintf(stderr, fmt, args);
    va_end(args);
    fputc('\n', stderr);
}

void Finding(const char* file, int line, const char* fmt, ...)
{
    char what[2048];
    va_list args;
    va_start(args, fmt);
    vsnprintf(what, sizeof(what), fmt, args);
    va_end(args);
    const char* base = strrchr(file, '/');
    char report[2400];
    int n = snprintf(report, sizeof(report), "FINDING: %s at %s:%d\n", what, base ? base + 1 : file, line);
    if (n < 0)
        n = 0;
    if (static_cast<size_t>(n) >= sizeof(report))
        n = sizeof(report) - 1;
    WriteAll(STDERR_FILENO, report, static_cast<size_t>(n));
    _exit(1);
}

}

int main(int argc, char** argv)
{
    using namespace Fuzz;

    uint64_t seed = 1;
    uint64_t iterations = 0;
    bool haveIterations = false;
    long seconds = -1;
    bool keepGoing = false;
    const char* replay = nullptr;
    for (int i = 1; i < argc; i++)
    {
        std::string a = argv[i];
        auto value = [&]() -> const char* {
            if (i + 1 >= argc)
                Usage();
            return argv[++i];
        };
        if (a == "--seed")
            seed = strtoull(value(), nullptr, 0);
        else if (a == "--iterations")
        {
            iterations = strtoull(value(), nullptr, 0);
            haveIterations = true;
        }
        else if (a == "--seconds")
            seconds = strtol(value(), nullptr, 0);
        else if (a == "--keep-going")
            keepGoing = true;
        else if (a == "--trace")
            Trace = true;
        else if (a == "--replay")
            replay = value();
        else
            Usage();
    }
    Counting = getenv("CPP_FUZZ_STATS") != nullptr;
    /* Nothing buffered twice: a forked child would print the parent's. */
    setvbuf(stdout, nullptr, _IOLBF, 0);

    if (replay != nullptr)
    {
        /* One input again, here, with the sanitizers' whole reports. */
        FILE* f = fopen(replay, "r");
        if (f == nullptr)
        {
            perror(replay);
            return 2;
        }
        std::string text;
        char buf[4096];
        size_t n;
        while ((n = fread(buf, 1, sizeof(buf), f)) > 0)
            text.append(buf, n);
        fclose(f);
        std::vector<uint8_t> data;
        if (!Unhex(text, data))
        {
            fprintf(stderr, "%s: not a hex file\n", replay);
            return 2;
        }
        RunInput(data);
        printf("%s: ran to its end\n", TheTarget.Name);
        return 0;
    }

    Progress = static_cast<Shared*>(mmap(nullptr, sizeof(Shared), PROT_READ | PROT_WRITE,
        MAP_SHARED | MAP_ANONYMOUS, -1, 0));
    if (Progress == MAP_FAILED)
    {
        perror("mmap");
        return 2;
    }

    Run run = {seed, haveIterations ? iterations : TheTarget.Gate, seconds, Now()};
    uint64_t iter = 0;
    uint64_t failed = 0;
    bool hung = false;
    std::map<std::string, uint64_t> reached;
    std::set<std::string> places;
    while (!run.Over(iter))
    {
        std::string message;
        int signal = 0;
        Outcome outcome = RunBatch(run, iter, message, signal, reached);
        if (outcome == Outcome::Pass)
        {
            iter = Progress->Done;
            break;
        }

        /* An input ended its batch: again, alone, for its own report. */
        uint64_t bad = Progress->Current;
        uint64_t batchStart = iter;
        std::vector<uint8_t> data = MakeInput(TheTarget.Name, seed, bad, TheTarget.MaxLen);
        std::string alone;
        int aloneSignal = 0;
        Outcome again = RunAlone(data, alone, aloneSignal);
        if (again != Outcome::Pass)
        {
            outcome = again;
            message = alone;
            signal = aloneSignal;
        }

        std::string what;
        if (outcome == Outcome::Hang)
        {
            hung = true;
            what = "a hang: no end in " + std::to_string(HangSeconds) + " s";
        }
        else if (outcome == Outcome::Crash)
        {
            std::string head = Headline(message);
            what = "killed by signal " + std::to_string(signal) + (head.empty() ? "" : ": " + head);
        }
        else
        {
            what = Headline(message);
            if (what.empty())
                what = "exit with a failure, and no report";
        }
        if (again == Outcome::Pass)
            what = "only after the inputs before it in its batch, from iteration " + std::to_string(batchStart) +
                   " -- the target's Reset leaves something behind: " + what;
        failed++;
        std::string place = (outcome == Outcome::Hang) ? "hang" : Place(what);
        if (places.insert(place).second)
        {
            char file[256];
            snprintf(file, sizeof(file), "cpp-fuzz-%s-%llu-%llu.hex", TheTarget.Name,
                static_cast<unsigned long long>(seed), static_cast<unsigned long long>(bad));
            FILE* f = fopen(file, "w");
            if (f != nullptr)
            {
                std::string h = Hex(data);
                fwrite(h.data(), 1, h.size(), f);
                fputc('\n', f);
                fclose(f);
            }
            printf("FAIL %s: seed %llu iteration %llu: %s\n", TheTarget.Name,
                static_cast<unsigned long long>(seed), static_cast<unsigned long long>(bad), what.c_str());
            printf("     input in %s -- %s --replay %s\n", file, TheTarget.Name, file);
        }
        if (!keepGoing)
            return hung ? 3 : 1;
        iter = bad + 1;
    }
    printf("%-10s %8llu inputs, %6.1f s", TheTarget.Name, static_cast<unsigned long long>(iter), Now() - run.Start);
    if (failed != 0)
        printf(", %llu failed", static_cast<unsigned long long>(failed));
    printf("\n");
    if (Counting)
    {
        for (const auto& r : reached)
            printf("           %6.2f%%  %s\n", 100.0 * r.second / (iter ? iter : 1), r.first.c_str());
    }
    if (!places.empty())
        return hung ? 3 : 1;
    return 0;
}
