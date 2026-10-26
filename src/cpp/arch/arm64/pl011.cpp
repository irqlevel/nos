#include "pl011.h"

#include <kernel/trace.h>
#include <hal/cpu.h>

namespace kernel
{

ulong Pl011::base_;
/* A function-local static, not a static member: its constructor registers it
   with the watchdog, as the x86 serial port's lock is, and a global's
   constructor is one this kernel never runs -- as a member it went unwatched
   from the day it was written. Built on the first line printed. */
static RawSpinLock& out_lock()
{
    static RawSpinLock lock;
    return lock;
}

namespace
{

inline void mmio_write32(ulong addr, u32 value)
{
    *reinterpret_cast<volatile u32*>(addr) = value;
}

inline u32 mmio_read32(ulong addr)
{
    return *reinterpret_cast<volatile u32*>(addr);
}

}

void Pl011::early_init(ulong virt_base)
{
    base_ = virt_base;

    mmio_write32(base_ + CR, 0);
    /* QEMU ignores the baud divisor; program 115200 @ 24MHz anyway */
    mmio_write32(base_ + IBRD, 13);
    mmio_write32(base_ + FBRD, 1);
    mmio_write32(base_ + LCR_H, LCR_H_FEN | LCR_H_WLEN8);
    mmio_write32(base_ + CR, CR_UARTEN | CR_TXE | CR_RXE);
}

void Pl011::put_char(char c)
{
    while (mmio_read32(base_ + FR) & FR_TXFF)
    {
    }
    mmio_write32(base_ + DR, (u32)(u8)c);
}

void Pl011::panic_print_string(const char* s)
{
    while (*s != '\0')
    {
        if (*s == '\n')
            put_char('\r');
        put_char(*s);
        s++;
    }
}

void Pl011::print_string(const char* s)
{
    ulong flags = hal::irq_save();
    out_lock().lock();
    panic_print_string(s);
    out_lock().unlock();
    hal::irq_restore(flags);
}

bool Pl011::setup(u32 int_id)
{
    /* PL011 interrupts are level-triggered */
    Interrupt::register_level(*this, int_id, int_id);
    mmio_write32(base_ + IMSC, INT_RX | INT_RT);
    return true;
}

bool Pl011::register_observer(SerialObserver& observer)
{
    if (observer_count_ >= MAX_OBSERVERS)
        return false;
    observers_[observer_count_] = &observer;
    observer_count_ = observer_count_ + 1;
    return true;
}

void Pl011::on_interrupt_register(u8 irq, u8 vector)
{
    (void)irq;
    int_vector_ = vector;
}

InterruptHandlerFn Pl011::get_handler_fn()
{
    return nullptr; /* dispatch is object-based on arm64 */
}

void Pl011::on_interrupt(Context* ctx)
{
    (void)ctx;

    while (!(mmio_read32(base_ + FR) & FR_RXFE))
    {
        char c = (char)(mmio_read32(base_ + DR) & 0xFF);
        if (c == '\r')
            c = '\n';
        for (ulong i = 0; i < observer_count_; i++)
            observers_[i]->on_char(c, (u8)c);
    }

    mmio_write32(base_ + ICR, INT_RX | INT_RT);
}

}
