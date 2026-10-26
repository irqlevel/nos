#pragma once

// Machine power control. x86: ACPI PM1a / QEMU debug-exit / keyboard
// controller / PCI reset register; arm64 later: PSCI SYSTEM_OFF/RESET.
namespace hal
{

void __attribute__((noreturn)) power_off();
void __attribute__((noreturn)) reset();

/* Reset() as the panic path needs it: nothing traced and no lock taken --
   the CPU calling it may hold any of them -- and on x86 a triple fault
   where nothing else answers. Reset() itself traces, then calls it. */
void __attribute__((noreturn)) reset_now();

}
