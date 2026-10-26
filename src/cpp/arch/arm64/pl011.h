#pragma once

#include <include/types.h>
#include <kernel/interrupt.h>
#include <kernel/raw_spin_lock.h>
#include <drivers/serial.h> /* SerialObserver interface */

namespace kernel
{

/* PL011 UART console (QEMU virt): polled TX, interrupt-driven RX with
   the same observer contract as the x86 Serial driver, so the shell
   works unchanged. Static early-output entry points stay usable from
   any context (panic included). */
class Pl011 final : public InterruptHandler
{
public:
    static Pl011& get_instance()
    {
        static Pl011 instance;
        return instance;
    }

    static void early_init(ulong virt_base);
    static void put_char(char c);

    /* Serialized against concurrent CPUs; IRQs off while held */
    static void print_string(const char* s);

    /* Panic path: no locks (the panicking CPU may hold them) */
    static void panic_print_string(const char* s);

    /* Enable the RX interrupt (INTID from the DTB) */
    bool setup(u32 int_id);

    bool register_observer(SerialObserver& observer);

    void on_interrupt_register(u8 irq, u8 vector) override;
    InterruptHandlerFn get_handler_fn() override;
    void on_interrupt(Context* ctx) override;

private:
    Pl011() = default;
    ~Pl011() = default;
    Pl011(const Pl011& other) = delete;
    Pl011(Pl011&& other) = delete;
    Pl011& operator=(const Pl011& other) = delete;
    Pl011& operator=(Pl011&& other) = delete;

    static ulong base_;

    static const ulong MAX_OBSERVERS = 4;
    SerialObserver* observers_[MAX_OBSERVERS] = {};
    ulong observer_count_ = 0;
    u8 int_vector_ = 0;

    /* Register offsets */
    static const ulong DR = 0x00;
    static const ulong FR = 0x18;
    static const ulong IBRD = 0x24;
    static const ulong FBRD = 0x28;
    static const ulong LCR_H = 0x2C;
    static const ulong CR = 0x30;
    static const ulong IMSC = 0x38;
    static const ulong ICR = 0x44;

    static const u32 FR_TXFF = 1 << 5;
    static const u32 FR_RXFE = 1 << 4;

    static const u32 LCR_H_FEN = 1 << 4;
    static const u32 LCR_H_WLEN8 = 3 << 5;

    static const u32 CR_UARTEN = 1 << 0;
    static const u32 CR_TXE = 1 << 8;
    static const u32 CR_RXE = 1 << 9;

    static const u32 INT_RX = 1 << 4;
    static const u32 INT_RT = 1 << 6;
};

}
