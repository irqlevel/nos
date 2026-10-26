#pragma once

// Interrupt-controller CPU-side operations. Provides namespace Hal {
// irq_eoi(), irq_eoi(vector), irq_is_in_service(vector), get_current_cpu_hw_id(),
// send_ipi(hw_id, vector), nmi_ipi_supported(), send_nmi_ipi(hw_id) }. IRQ *routing* (route/mask a line to a CPU) is
// deliberately not abstracted yet: drivers request lines through
// Interrupt::register_handler/register_level, whose backend stays arch code until
// the second irqchip exists.
#if defined(__x86_64__)
#include <arch/x86_64/hal_irqchip_inline.h>
#elif defined(__aarch64__)
#include <arch/arm64/hal_irqchip_inline.h>
#else
#error "unsupported architecture"
#endif
