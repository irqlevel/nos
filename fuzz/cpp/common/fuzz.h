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

namespace Fuzz
{

/* The input, read as a stream of choices: past its end every read is 0,
   which is always the ordinary choice, and More() says there is no more.
   The same reader as fuzz/common/input.rs. */
class Input
{
public:
    Input(const uint8_t* data, size_t size)
        : Data(data)
        , Size(size)
        , At(0)
    {
    }

    bool More() const { return At < Size; }

    uint8_t U8()
    {
        uint8_t b = (At < Size) ? Data[At] : 0;
        At++;
        return b;
    }
    uint16_t U16() { uint16_t lo = U8(); return static_cast<uint16_t>(lo | (U8() << 8)); }
    uint32_t U32() { uint32_t lo = U16(); return lo | (static_cast<uint32_t>(U16()) << 16); }
    uint64_t U64() { uint64_t lo = U32(); return lo | (static_cast<uint64_t>(U32()) << 32); }
    bool Bool() { return (U8() & 1) != 0; }

    /* True about n times in 256: never past the end, whose zeros are the
       ordinary choice. */
    bool Chance(uint8_t n) { return More() && U8() < n; }

    /* A number below n (0 for none). */
    uint64_t Below(uint64_t n)
    {
        if (n == 0)
            return 0;
        if (n <= 256)
            return U8() % n;
        if (n <= (1u << 16))
            return U16() % n;
        return U64() % n;
    }

    /* A number in [lo, hi]. */
    uint64_t Range(uint64_t lo, uint64_t hi) { return lo + Below(hi - lo + 1); }

    template <typename T, size_t N>
    T Pick(const T (&xs)[N]) { return xs[Below(N)]; }

    /* A 32-bit value: an edge, a power of two, or anything. */
    uint32_t Value32();
    /* A 64-bit value, the same way. */
    uint64_t Value64();

    std::vector<uint8_t> Bytes(size_t n)
    {
        std::vector<uint8_t> v(n);
        for (size_t i = 0; i < n; i++)
            v[i] = U8();
        return v;
    }

private:
    const uint8_t* Data;
    size_t Size;
    size_t At;
};

/* n bytes from seed: varied, and not the input's to spend. */
std::vector<uint8_t> Noise(uint32_t seed, size_t n);

/* What the runner runs: bytes in; a finding out, or nothing. Inputs run
   one after another in a process forked for a batch of them, and Reset puts
   back, before each, whatever of the kernel's the one before may have
   changed: a finding is then run again alone, in a process of its own, and
   one that does not come back there is Reset's own bug. */
struct Target
{
    const char* Name;
    void (*Reset)();
    void (*Run)(Input& input);
    /* How long an input is at most. */
    size_t MaxLen;
    /* How many inputs the gate runs of it: as many as reach its deep
       states, the whole gate a couple of minutes. */
    uint64_t Gate;
};

/* Each target's file defines it. */
extern const Target TheTarget;

/* That this input reached what -- counted once an input, and reported by
   CPP_FUZZ_STATS=1, so that a target that stops reaching somewhere says
   so. */
void Reached(const char* what);

/* A finding: the input's process reports it, with the place it was found,
   and ends. */
[[noreturn]] void Finding(const char* file, int line, const char* fmt, ...)
    __attribute__((format(printf, 3, 4)));

/* A line of the input's story, printed under --trace. */
void Say(const char* fmt, ...) __attribute__((format(printf, 1, 2)));
bool Tracing();

}

/* Something the code did that it must not. */
#define INVARIANT(cond, ...)                                    \
    do {                                                        \
        if (!(cond))                                            \
            Fuzz::Finding(__FILE__, __LINE__, __VA_ARGS__);      \
    } while (false)
