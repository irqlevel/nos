#pragma once

// Interrupt-controller CPU-side operations. Provides namespace Hal {
// IrqEoi(), IrqEoi(vector), IrqIsInService(vector), GetCurrentCpuHwId(),
// SendIpi(hwId, vector) }. IRQ *routing* (route/mask a line to a CPU) is
// deliberately not abstracted yet: drivers request lines through
// Interrupt::Register/RegisterLevel, whose backend stays arch code until
// the second irqchip exists.
#if defined(__x86_64__)
#include <arch/x86_64/hal_irqchip_inline.h>
#else
#error "unsupported architecture"
#endif
