#include "pl011.h"

namespace Kernel
{

ulong Pl011::Base;

static inline void MmioWrite32(ulong addr, u32 value)
{
    *reinterpret_cast<volatile u32*>(addr) = value;
}

static inline u32 MmioRead32(ulong addr)
{
    return *reinterpret_cast<volatile u32*>(addr);
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

}
