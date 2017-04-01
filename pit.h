#pragma once

#include "types.h"
#include "atomic.h"

namespace Kernel
{

namespace Core
{

class Pit final
{
public:
    static Pit& GetInstance()
    {
        static Pit instance;

        return instance;
    }

    void RegisterInterrupt(int intNum);
    void UnregisterInterrupt();

    void Interrupt();

private:
    Pit();
    ~Pit();

    Pit(const Pit& other) = delete;
    Pit(Pit&& other) = delete;
    Pit& operator=(const Pit& other) = delete;
    Pit& operator=(Pit&& other) = delete;

    int IntNum;

    Atomic IntCounter;
};


};

}