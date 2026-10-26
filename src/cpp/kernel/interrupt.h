#pragma once

#include <lib/stdlib.h>

#include "atomic.h"

namespace kernel
{

using InterruptHandlerFn = void (*)();

struct Context;

class InterruptHandler
{
public:
    virtual void on_interrupt_register(u8 irq, u8 vector) = 0;
    virtual InterruptHandlerFn get_handler_fn() = 0;

    /* Called by shared interrupt dispatch. Override to handle the interrupt.
       Must NOT signal EOI -- the shared dispatcher does that. */
    virtual void on_interrupt(Context* ctx) { (void)ctx; }
};

enum InterruptSource : u8
{
    IrqPit = 0,
    IrqHpet,
    IrqIO8042,
    IrqSerial,
    IrqIPI,
    IrqLapicTimer,
    IrqShared,
    IrqMsix,
    IrqDummy,
    IrqSpurious,
    IrqPosted,
    IrqTimerWake,
    IrqMax,
};

class InterruptStats
{
public:
    static void inc(InterruptSource src);
    static long get(InterruptSource src);
    static const char* get_name(InterruptSource src);
    static constexpr ulong COUNT = IrqMax;

private:
    InterruptStats() = delete;
    ~InterruptStats() = delete;

    static Atomic counters_[IrqMax];
};

class Interrupt
{
public:
    /* Edge-triggered system IRQs (PIT/HPET/8042/serial). These stay
       pinned to the registering CPU (the BSP): timekeeping and the
       scheduling tick depend on it. Not balanced by IrqBalance. */
    static void register_handler(InterruptHandler& handler, u8 irq, u8 vector);

    /* Level-triggered device IRQs (PCI INTx). The destination CPU is
       chosen by IrqBalance and spread across CPUs after SMP bringup.
       MSI-X vectors are likewise balanced via MsixTable::EnableVector. */
    static void register_level(InterruptHandler& handler, u8 irq, u8 vector);

    /* Shared interrupt dispatch (called from SharedInterruptStub) */
    static void shared_dispatch(Context* ctx);

private:
    Interrupt() = delete;
    ~Interrupt() = delete;
    Interrupt(const Interrupt& other) = delete;
    Interrupt(Interrupt&& other) = delete;
    Interrupt& operator=(const Interrupt& other) = delete;
    Interrupt& operator=(Interrupt&& other) = delete;

    /* Shared handler table: multiple handlers can share a vector */
    static const ulong MAX_SHARED_HANDLERS = 8;

    static const ulong MAX_VECTORS = 256;
    struct VectorEntry
    {
        u8 gsi;
        u8 handler_count;
        /* True for RegisterLevel (PCI INTx) entries; only those may chain.
           Edge entries (Register) own their GSI + vector exclusively. */
        bool level;
        InterruptHandler* handlers[MAX_SHARED_HANDLERS];
    };

    static VectorEntry vectors_[MAX_VECTORS];
};

}