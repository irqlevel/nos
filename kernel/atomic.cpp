#include "atomic.h"
#include "asm.h"

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