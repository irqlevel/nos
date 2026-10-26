// The C++ fuzzers' side of a target: the input it reads its choices from, how
// it reports what it found, and the states it counts. A target is one binary
// (fuzz/cpp/Makefile): the kernel sources it tests, compiled for the host as
// they are, the stand-ins for the kernel around them (kernel.cpp, heap.cpp,
// the host HAL in fuzz/cpp/host), and the target's own file, which defines
// Fuzz::TheTarget. The runner (runner.cpp) feeds it random bytes from a seed,
// each input in a process of its own forked from one that has run nothing.
//
// A file of the fuzzers includes host.h before anything else (host.h says
// why), then the kernel's headers, then this one.
#pragma once

#include "host.h"

namespace fuzz
{

/* The input, read as a stream of choices: past its end every read is 0,
   which is always the ordinary choice, and More() says there is no more.
   The same reader as fuzz/common/input.rs. */
class Input
{
public:
    Input(const uint8_t* data, size_t size)
        : data_(data)
        , size_(size)
        , at_(0)
    {
    }

    bool more() const { return at_ < size_; }

    uint8_t u8()
    {
        uint8_t b = (at_ < size_) ? data_[at_] : 0;
        at_++;
        return b;
    }
    uint16_t u16() { uint16_t lo = u8(); return static_cast<uint16_t>(lo | (u8() << 8)); }
    uint32_t u32() { uint32_t lo = u16(); return lo | (static_cast<uint32_t>(u16()) << 16); }
    uint64_t u64() { uint64_t lo = u32(); return lo | (static_cast<uint64_t>(u32()) << 32); }
    bool boolean() { return (u8() & 1) != 0; }

    /* True about n times in 256: never past the end, whose zeros are the
       ordinary choice. */
    bool chance(uint8_t n) { return more() && u8() < n; }

    /* A number below n (0 for none). */
    uint64_t below(uint64_t n)
    {
        if (n == 0)
            return 0;
        if (n <= 256)
            return u8() % n;
        if (n <= (1u << 16))
            return u16() % n;
        return u64() % n;
    }

    /* A number in [lo, hi]. */
    uint64_t range(uint64_t lo, uint64_t hi) { return lo + below(hi - lo + 1); }

    template <typename T, size_t N>
    T pick(const T (&xs)[N]) { return xs[below(N)]; }

    /* A 32-bit value: an edge, a power of two, or anything. */
    uint32_t value32();
    /* A 64-bit value, the same way. */
    uint64_t value64();

    std::vector<uint8_t> bytes(size_t n)
    {
        std::vector<uint8_t> v(n);
        for (size_t i = 0; i < n; i++)
            v[i] = u8();
        return v;
    }

private:
    const uint8_t* data_;
    size_t size_;
    size_t at_;
};

/* n bytes from seed: varied, and not the input's to spend. */
std::vector<uint8_t> noise(uint32_t seed, size_t n);

/* What the runner runs: bytes in; a finding out, or nothing. Inputs run
   one after another in a process forked for a batch of them, and Reset puts
   back, before each, whatever of the kernel's the one before may have
   changed: a finding is then run again alone, in a process of its own, and
   one that does not come back there is Reset's own bug. */
struct Target
{
    const char* name;
    void (*reset)();
    void (*run)(Input& input);
    /* How long an input is at most. */
    size_t max_len;
    /* How many inputs the gate runs of it: as many as reach its deep
       states, the whole gate a couple of minutes. */
    uint64_t gate;
};

/* Each target's file defines it. */
extern const Target THE_TARGET;

/* That this input reached what -- counted once an input, and reported by
   CPP_FUZZ_STATS=1, so that a target that stops reaching somewhere says
   so. */
void reached(const char* what);

/* A finding: the input's process reports it, with the place it was found,
   and ends. */
[[noreturn]] void finding(const char* file, int line, const char* fmt, ...)
    __attribute__((format(printf, 3, 4)));

/* A line of the input's story, printed under --trace. */
void say(const char* fmt, ...) __attribute__((format(printf, 1, 2)));
bool tracing();

}

/* Something the code did that it must not. */
#define INVARIANT(cond, ...)                                    \
    do {                                                        \
        if (!(cond))                                            \
            fuzz::finding(__FILE__, __LINE__, __VA_ARGS__);      \
    } while (false)
