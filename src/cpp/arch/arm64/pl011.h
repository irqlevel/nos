#pragma once

#include <include/types.h>

namespace Kernel
{

/* Minimal PL011 UART console (QEMU virt). M1: polled TX only; the
   interrupt-driven Serial-shaped driver arrives with the GIC milestone. */
class Pl011 final
{
public:
    static void EarlyInit(ulong virtBase);
    static void PutChar(char c);
    static void PrintString(const char* s);

private:
    static ulong Base;

    /* Register offsets */
    static const ulong Dr = 0x00;
    static const ulong Fr = 0x18;
    static const ulong Ibrd = 0x24;
    static const ulong Fbrd = 0x28;
    static const ulong LcrH = 0x2C;
    static const ulong Cr = 0x30;

    static const u32 FrTxff = 1 << 5;

    static const u32 LcrHFen = 1 << 4;
    static const u32 LcrHWlen8 = 3 << 5;

    static const u32 CrUarten = 1 << 0;
    static const u32 CrTxe = 1 << 8;
    static const u32 CrRxe = 1 << 9;
};

}
