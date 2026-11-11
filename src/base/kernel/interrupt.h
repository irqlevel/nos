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
       scheduling tick depend on it. Not balanced by IrqBalance.

       False, which is said, when the handler was not registered -- the
       line or the vector is somebody's already, or the line is none the
       interrupt controller has: nothing was programmed, and the handler's
       on_interrupt_register was not called. */
    static bool register_handler(InterruptHandler& handler, u8 irq, u8 vector);

    /* Level-triggered device IRQs (PCI INTx). The destination CPU is
       chosen by IrqBalance and spread across CPUs after SMP bringup.
       MSI-X vectors are likewise balanced via MsixTable::enable_vector.
       False as register_handler's. */
    static bool register_level(InterruptHandler& handler, u8 irq, u8 vector);

    /* A handler register_level registered, taken off its line. The line's
       last handler gone, the line is masked at the interrupt controller --
       a device still asserting it would otherwise interrupt for ever, with
       nobody left to quiet it -- and its vector is free to be registered
       at again; the others of a shared line go on being called.

       The caller serializes this with register_level, and the handler
       object outlives every dispatch that may have read it: one in flight
       on another CPU may still call its on_interrupt once this returns. */
    static void unregister_level(InterruptHandler& handler);

    /* Whether a line registered at this vector would be the first there.
       On arm64, where a line's vector is its INTID and the one asked for
       is not looked at, always. */
    static bool is_vector_free(u8 vector);

    /* Shared interrupt dispatch (called from shared_interrupt_stub) */
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
        /* True for register_level (PCI INTx) entries; only those may chain.
           Edge entries (Register) own their GSI + vector exclusively. */
        bool level;
        /* The vector's IDT entry is the shared stub: set when a second
           handler joins the line, and kept until the last one leaves --
           shared_dispatch calls the handlers of such vectors, however many
           are left */
        bool shared;
        InterruptHandler* handlers[MAX_SHARED_HANDLERS];
    };

    /* handler_count, shared and handlers are read by shared_dispatch on any
       CPU while a registration changes them: every access of those three is
       an atomic one, a handler stored before the count that reaches it. */

    static VectorEntry vectors_[MAX_VECTORS];
};

}