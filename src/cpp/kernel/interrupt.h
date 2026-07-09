#pragma once

#include <lib/stdlib.h>

#include "atomic.h"

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

enum InterruptSource : u8
{
    IrqPit = 0,
    IrqHpet,
    IrqIO8042,
    IrqSerial,
    IrqVirtioBlk,
    IrqVirtioNet,
    IrqVirtioScsi,
    IrqIPI,
    IrqShared,
    IrqMsix,
    IrqDummy,
    IrqSpurious,
    IrqMax,
};

class InterruptStats
{
public:
    static void Inc(InterruptSource src);
    static long Get(InterruptSource src);
    static const char* GetName(InterruptSource src);
    static constexpr ulong Count = IrqMax;

private:
    InterruptStats() = delete;
    ~InterruptStats() = delete;

    static Atomic Counters[IrqMax];
};

class Interrupt
{
public:
    /* Edge-triggered system IRQs (PIT/HPET/8042/serial). These stay
       pinned to the registering CPU (the BSP): timekeeping and the
       scheduling tick depend on it. Not balanced by IrqBalance. */
    static void Register(InterruptHandler& handler, u8 irq, u8 vector);

    /* Level-triggered device IRQs (PCI INTx). The destination CPU is
       chosen by IrqBalance and spread across CPUs after SMP bringup.
       MSI-X vectors are likewise balanced via MsixTable::EnableVector. */
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
        /* True for RegisterLevel (PCI INTx) entries; only those may chain.
           Edge entries (Register) own their GSI + vector exclusively. */
        bool Level;
        InterruptHandler* Handlers[MaxSharedHandlers];
    };

    static VectorEntry Vectors[MaxVectors];
};

}