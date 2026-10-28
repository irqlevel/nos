#pragma once

// Hardware console sink for kernel log output. The arch implementation
// owns the sink policy (x86: 8250 serial + optional VGA mirror; arm64
// later: PL011). Interactive console *input* is not a HAL concern -- it
// stays with the serial driver's observer interface.
namespace stdlib
{
class Printer;
}

namespace hal
{

/* Dump arch CPU state (registers/control state) to the given printer;
   used by the shell "cpu" command. Defined per arch. */
void print_cpu_state(stdlib::Printer& out);

/* What this machine is, and which of the things the kernel actually depends
   on it provides. Written for the moment a new box is booted for the first
   time: the answer to "will this work here" is a handful of feature bits,
   and reading them off the running kernel beats inferring them from a
   model number. Defined per arch. */
void print_cpu_info(stdlib::Printer& out);

/* What the CPU is called, NUL-terminated into buf, cut to fit: x86's brand
   string, arm64's implementer and part -- the line a report heads its CPU
   with. Defined per arch. */
void cpu_model(char* buf, unsigned long len);

void console_write(const char *msg);

/* Panic-context variant: must be usable with interrupts off and locks
   held (polled output only). */
void console_panic_write(const char *msg);

/* Interactive console (the shell's Printer sink): honors the arch's
   console selection policy (x86: console=serial/vga parameters over
   8250+VGA; arm64: PL011). */
void console_out(const char *s);
void console_out_backspace();
void console_out_clear();

}
