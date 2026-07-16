#pragma once

#include <include/types.h>
#include <kernel/interrupt.h>
#include <drivers/serial.h> /* SerialObserver interface */

namespace Kernel
{

/* PL011 UART console (QEMU virt): polled TX, interrupt-driven RX with
   the same observer contract as the x86 Serial driver, so the shell
   works unchanged. Static early-output entry points stay usable from
   any context (panic included). */
class Pl011 final : public InterruptHandler
{
public:
    static Pl011& GetInstance()
    {
        static Pl011 Instance;
        return Instance;
    }

    static void EarlyInit(ulong virtBase);
    static void PutChar(char c);
    static void PrintString(const char* s);

    /* Enable the RX interrupt (INTID from the DTB) */
    bool Setup(u32 intId);

    bool RegisterObserver(SerialObserver& observer);

    void OnInterruptRegister(u8 irq, u8 vector) override;
    InterruptHandlerFn GetHandlerFn() override;
    void OnInterrupt(Context* ctx) override;

private:
    Pl011() = default;
    ~Pl011() = default;
    Pl011(const Pl011& other) = delete;
    Pl011(Pl011&& other) = delete;
    Pl011& operator=(const Pl011& other) = delete;
    Pl011& operator=(Pl011&& other) = delete;

    static ulong Base;

    static const ulong MaxObservers = 4;
    SerialObserver* Observers[MaxObservers] = {};
    ulong ObserverCount = 0;
    u8 IntVector = 0;

    /* Register offsets */
    static const ulong Dr = 0x00;
    static const ulong Fr = 0x18;
    static const ulong Ibrd = 0x24;
    static const ulong Fbrd = 0x28;
    static const ulong LcrH = 0x2C;
    static const ulong Cr = 0x30;
    static const ulong Imsc = 0x38;
    static const ulong Icr = 0x44;

    static const u32 FrTxff = 1 << 5;
    static const u32 FrRxfe = 1 << 4;

    static const u32 LcrHFen = 1 << 4;
    static const u32 LcrHWlen8 = 3 << 5;

    static const u32 CrUarten = 1 << 0;
    static const u32 CrTxe = 1 << 8;
    static const u32 CrRxe = 1 << 9;

    static const u32 IntRx = 1 << 4;
    static const u32 IntRt = 1 << 6;
};

}
