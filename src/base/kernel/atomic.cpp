#include "atomic.h"
#include <hal/cpu.h>

#include <kernel/panic.h>

namespace kernel
{

void Atomic::inc()
{
    atomic_inc(&value_);
}

void Atomic::dec()
{
    atomic_dec(&value_);
}

void Atomic::add(long delta)
{
    atomic_add(&value_, delta);
}

bool Atomic::dec_and_test()
{
    long old_value = atomic_read_and_dec(&value_);
    return (old_value == 1) ? true : false;
}

void Atomic::set(long value)
{
    atomic_write(&value_, value);
}

long Atomic::get()
{
    return atomic_read(&value_);
}

bool Atomic::set_bit(ulong bit)
{
    BUG_ON(bit >= stdlib::size_of_in_bits<long>());

    return (atomic_test_and_set_bit(&value_, bit)) ? true : false;
}

bool Atomic::clear_bit(ulong bit)
{
    BUG_ON(bit >= stdlib::size_of_in_bits<long>());

    return (atomic_test_and_clear_bit(&value_, bit)) ? true : false;
}

bool Atomic::test_bit(ulong bit)
{
    BUG_ON(bit >= stdlib::size_of_in_bits<long>());

    return (atomic_test_bit(&value_, bit)) ? true : false;
}

long Atomic::cmpxchg(long exchange, long comparand)
{
    return atomic_cmpxchg(&value_, exchange, comparand);
}

Atomic::Atomic(Atomic&& other)
{
    set(other.get());
}

Atomic& Atomic::operator=(Atomic&& other)
{
    set(other.get());
    return *this;
}

}