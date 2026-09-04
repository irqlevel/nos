#pragma once

#include <lib/stdlib.h>
#include <lib/printer.h>

namespace Kernel
{

/* How deep the stacks have actually gone.
 
   Not by sampling: a profiler tick sees only the instants it fires at, and a
   deep excursion lasting microseconds is missed essentially every time --
   what comes back is a lower bound on the maximum, and a weak one.
 
   By poisoning instead. A stack is filled with a known pattern when it is
   created; it grows downward and overwrites that pattern from the top, so
   the pattern survives exactly in the region the stack never reached.
   Counting the intact words from the base up gives the true high-water mark
   over the whole life of the stack, including spikes during boot, and costs
   nothing at all while the machine runs -- one fill at creation, and a scan
   only when someone asks.
 
   It errs in one direction: a function that reserves a large frame and does
   not write all of it leaves pattern inside territory that was in fact used,
   so the scan can report more free space than there really was. The same
   limitation applies to Linux's CONFIG_DEBUG_STACK_USAGE, which is the same
   trick. */
namespace StackProbe
{

/* 0x5A repeated: a byte fill, so the fast MemSet writes it, and a word value
   that is neither a plausible pointer nor a plausible small integer. */
static const u8 PatternByte = 0x5A;
static const ulong Pattern = 0x5A5A5A5A5A5A5A5AUL;

/* Both take an 8-byte aligned base and a length that is a multiple of 8. */
void Poison(void* base, ulong size);

/* Bytes of pattern still intact, counted from the base upward -- the part of
   the stack that was never touched. */
ulong Untouched(const void* base, ulong size);

}

/* Defined in main.cpp, where the per-CPU static stacks live. */
void ReportCpuStacks(Stdlib::Printer& printer, ulong& worstFree);

}
