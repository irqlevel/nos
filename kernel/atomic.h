#pragma once

namespace Kernel
{

class Atomic
{
public:
    Atomic();
    Atomic(long value);
    void Inc();
    void Dec();
    bool DecAndTest();
    long Get();
    void Set(long value);
    ~Atomic();

    Atomic& operator=(Atomic&& other);
    Atomic(Atomic&& other);

private:
    Atomic(const Atomic& other) = delete;
    Atomic& operator=(const Atomic& other) = delete;

    volatile long Value;
};

}
