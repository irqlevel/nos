#pragma once

#include <lib/stdlib.h>

namespace Kernel
{

using InterruptHandlerFn = void (*)();

struct Context;

class InterruptHandler
{
public:
    virtual void OnInterruptRegister(u8 irq, u8 vector) = 0;
    virtual InterruptHandlerFn GetHandlerFn() = 0;

    /* Called by shared interrupt dispatch. Override to handle the interrupt.
       Must NOT call Lapic::EOI() -- the shared dispatcher does that. */
    virtual void OnInterrupt(Context* ctx) { (void)ctx; }
};

class Interrupt
{
public:
    static void Register(InterruptHandler& handler, u8 irq, u8 vector);
    static void RegisterLevel(InterruptHandler& handler, u8 irq, u8 vector);

    /* Shared interrupt dispatch (called from SharedInterruptStub) */
    static void SharedDispatch(Context* ctx);

private:
    Interrupt() = delete;
    ~Interrupt() = delete;
    Interrupt(const Interrupt& other) = delete;
    Interrupt(Interrupt&& other) = delete;
    Interrupt& operator=(const Interrupt& other) = delete;
    Interrupt& operator=(Interrupt&& other) = delete;

    /* Shared handler table: multiple handlers can share a vector */
    static const ulong MaxSharedHandlers = 8;

    static const ulong MaxVectors = 256;
    struct VectorEntry
    {
        u8 Gsi;
        u8 HandlerCount;
        InterruptHandler* Handlers[MaxSharedHandlers];
    };

    static VectorEntry Vectors[MaxVectors];
};

}