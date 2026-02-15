#include "atomic.h"
#include "asm.h"

#include <kernel/panic.h>

namespace Kernel
{

Atomic::Atomic()
{
    Set(0);
}

Atomic::Atomic(long value)
{
    Set(value);
}

void Atomic::Inc()
{
    AtomicInc(&Value);
}

void Atomic::Dec()
{
    AtomicDec(&Value);
}

void Atomic::Add(long delta)
{
    AtomicAdd(&Value, delta);
}

bool Atomic::DecAndTest()
{
    long oldValue = AtomicReadAndDec(&Value);
    return (oldValue == 1) ? true : false;
}

void Atomic::Set(long value)
{
    AtomicWrite(&Value, value);
}

long Atomic::Get()
{
    return AtomicRead(&Value);
}

bool Atomic::SetBit(ulong bit)
{
    BugOn(bit >= Stdlib::SizeOfInBits<long>());

    return (AtomicTestAndSetBit(&Value, bit)) ? true : false;
}

bool Atomic::ClearBit(ulong bit)
{
    BugOn(bit >= Stdlib::SizeOfInBits<long>());

    return (AtomicTestAndClearBit(&Value, bit)) ? true : false;
}

bool Atomic::TestBit(ulong bit)
{
    BugOn(bit >= Stdlib::SizeOfInBits<long>());

    return (AtomicTestBit(&Value, bit)) ? true : false;
}

long Atomic::Cmpxchg(long exchange, long comparand)
{
    return AtomicCmpxchg(&Value, exchange, comparand);
}

Atomic::~Atomic()
{
}

Atomic::Atomic(Atomic&& other)
{
    Set(other.Get());
}

Atomic& Atomic::operator=(Atomic&& other)
{
    Set(other.Get());
    return *this;
}

}