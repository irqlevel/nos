#include "pl011.h"

#include <kernel/trace.h>

namespace Kernel
{

ulong Pl011::Base;

namespace
{

inline void MmioWrite32(ulong addr, u32 value)
{
    *reinterpret_cast<volatile u32*>(addr) = value;
}

inline u32 MmioRead32(ulong addr)
{
    return *reinterpret_cast<volatile u32*>(addr);
}

}

void Pl011::EarlyInit(ulong virtBase)
{
    Base = virtBase;

    MmioWrite32(Base + Cr, 0);
    /* QEMU ignores the baud divisor; program 115200 @ 24MHz anyway */
    MmioWrite32(Base + Ibrd, 13);
    MmioWrite32(Base + Fbrd, 1);
    MmioWrite32(Base + LcrH, LcrHFen | LcrHWlen8);
    MmioWrite32(Base + Cr, CrUarten | CrTxe | CrRxe);
}

void Pl011::PutChar(char c)
{
    while (MmioRead32(Base + Fr) & FrTxff)
    {
    }
    MmioWrite32(Base + Dr, (u32)(u8)c);
}

void Pl011::PrintString(const char* s)
{
    while (*s != '\0')
    {
        if (*s == '\n')
            PutChar('\r');
        PutChar(*s);
        s++;
    }
}

bool Pl011::Setup(u32 intId)
{
    /* PL011 interrupts are level-triggered */
    Interrupt::RegisterLevel(*this, intId, intId);
    MmioWrite32(Base + Imsc, IntRx | IntRt);
    return true;
}

bool Pl011::RegisterObserver(SerialObserver& observer)
{
    if (ObserverCount >= MaxObservers)
        return false;
    Observers[ObserverCount] = &observer;
    ObserverCount = ObserverCount + 1;
    return true;
}

void Pl011::OnInterruptRegister(u8 irq, u8 vector)
{
    (void)irq;
    IntVector = vector;
}

InterruptHandlerFn Pl011::GetHandlerFn()
{
    return nullptr; /* dispatch is object-based on arm64 */
}

void Pl011::OnInterrupt(Context* ctx)
{
    (void)ctx;

    while (!(MmioRead32(Base + Fr) & FrRxfe))
    {
        char c = (char)(MmioRead32(Base + Dr) & 0xFF);
        if (c == '\r')
            c = '\n';
        for (ulong i = 0; i < ObserverCount; i++)
            Observers[i]->OnChar(c, (u8)c);
    }

    MmioWrite32(Base + Icr, IntRx | IntRt);
}

}
