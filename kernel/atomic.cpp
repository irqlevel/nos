#include "atomic.h"

#include <lib/lock.h>

namespace Kernel
{

namespace Core
{

Atomic::Atomic()
{
    Set(0);
}

Atomic::Atomic(int value)
{
    Set(value);
}

void Atomic::Inc()
{
    Shared::AutoLock lock(Lock);

    Value++;
}

bool Atomic::DecAndTest()
{
    Shared::AutoLock lock(Lock);

    Value--;
    return (Value == 0) ? true : false;
}

void Atomic::Set(int value)
{
    Shared::AutoLock lock(Lock);

    Value = value;
}

int Atomic::Get()
{
    Shared::AutoLock lock(Lock);

    return Value;
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
}