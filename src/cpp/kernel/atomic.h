#pragma once

#include <lib/stdlib.h>

namespace kernel
{

class Atomic final
{
public:
    /* constexpr, and the destructor trivial: an Atomic with static storage
       is then initialised by the compiler rather than by a constructor the
       kernel would never run (no .init_array here; the link refuses one).
       Plain initialisation, not Set(): nothing can see the object before
       its constructor returns. */
    constexpr Atomic()
        : value_(0)
    {
    }

    constexpr Atomic(long value)
        : value_(value)
    {
    }
    void inc();
    void dec();
    void add(long delta);
    bool dec_and_test();
    long get();
    void set(long value);
    bool set_bit(ulong bit);
    bool clear_bit(ulong bit);
    bool test_bit(ulong bit);

    long cmpxchg(long exchange, long comparand);

    ~Atomic() = default;

    Atomic& operator=(Atomic&& other);
    Atomic(Atomic&& other);

private:
    Atomic(const Atomic& other) = delete;
    Atomic& operator=(const Atomic& other) = delete;

    volatile long value_;
};

static_assert(sizeof(Atomic) == sizeof(long), "Invalid size");

}
