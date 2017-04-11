#pragma once

#include <lib/stdlib.h>

namespace Kernel
{

namespace Core
{

using InterruptHandlerFn = void (*)();

class InterruptHandler
{
public:
    virtual void OnInterruptRegister(u8 irq, u8 vector) = 0;
    virtual InterruptHandlerFn GetHandlerFn() = 0;
};

class Interrupt
{
public:
    static void Register(InterruptHandler& handler, u8 irq, u8 vector);
private:
    Interrupt() = delete;
    ~Interrupt() = delete;
    Interrupt(const Interrupt& other) = delete;
    Interrupt(Interrupt&& other) = delete;
    Interrupt& operator=(const Interrupt& other) = delete;
    Interrupt& operator=(Interrupt&& other) = delete;
};

}
}