#pragma once

// Hardware console sink for kernel log output. The arch implementation
// owns the sink policy (x86: 8250 serial + optional VGA mirror; arm64
// later: PL011). Interactive console *input* is not a HAL concern -- it
// stays with the serial driver's observer interface.
namespace Hal
{

void ConsoleWrite(const char *msg);

/* Panic-context variant: must be usable with interrupts off and locks
   held (polled output only). */
void ConsolePanicWrite(const char *msg);

}
