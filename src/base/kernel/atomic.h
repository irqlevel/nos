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
       Plain initialisation, not set(): nothing can see the object before
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

    /* The weaker orders, inline, for a lock word and what only a report
       reads. get and set are sequentially consistent and calls: set is a
       `lock xchg` on x86-64, a full barrier, and a spinlock's release made
       of three of them cost more than the cmpxchg that took it.
       get_relaxed: the value, ordered against nothing else -- what a
       waiter polls before it tries a cmpxchg, which orders for itself.
       set_release: every load and store before it done first, as a lock's
       release needs (a plain MOV on x86-64, STLR on arm64). set_relaxed: a
       value only a report reads. */
    long get_relaxed() const
    {
        return __atomic_load_n(&value_, __ATOMIC_RELAXED);
    }

    void set_release(long value)
    {
        __atomic_store_n(&value_, value, __ATOMIC_RELEASE);
    }

    void set_relaxed(long value)
    {
        __atomic_store_n(&value_, value, __ATOMIC_RELAXED);
    }

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
