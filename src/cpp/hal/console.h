#pragma once

// Hardware console sink for kernel log output. The arch implementation
// owns the sink policy (x86: 8250 serial + optional VGA mirror; arm64
// later: PL011). Interactive console *input* is not a HAL concern -- it
// stays with the serial driver's observer interface.
namespace Stdlib
{
class Printer;
}

namespace Hal
{

/* Dump arch CPU state (registers/control state) to the given printer;
   used by the shell "cpu" command. Defined per arch. */
void PrintCpuState(Stdlib::Printer& out);

void ConsoleWrite(const char *msg);

/* Panic-context variant: must be usable with interrupts off and locks
   held (polled output only). */
void ConsolePanicWrite(const char *msg);

/* Interactive console (the shell's Printer sink): honors the arch's
   console selection policy (x86: console=serial/vga parameters over
   8250+VGA; arm64: PL011). */
void ConsoleOut(const char *s);
void ConsoleOutBackspace();
void ConsoleOutClear();

}
