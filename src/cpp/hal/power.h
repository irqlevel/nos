#pragma once

// Machine power control. x86: ACPI PM1a / QEMU debug-exit / keyboard
// controller / PCI reset register; arm64 later: PSCI SYSTEM_OFF/RESET.
namespace Hal
{

void __attribute__((noreturn)) PowerOff();
void __attribute__((noreturn)) Reset();

}
