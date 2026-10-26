// The Hal:: interrupt-controller wrappers of hal/irqchip.h, for a process on
// the host: no controller, and the CPU the fuzzed code runs on is whichever
// the target says (Fuzz::CurrentCpu). An interrupt or an IPI sent is a
// finding: nothing the fuzzed code does may need one.
#pragma once

#include <include/types.h>

namespace fuzz
{
extern ulong current_cpu;
[[noreturn]] void host_hal_unreachable(const char* what);
}

namespace hal
{

/* IPI vector: IDT slot on x86, SGI INTID on arm64; on the host, the x86's */
constexpr u8 IPI_VECTOR = 0xFE;

static inline bool irq_chip_ready()
{
    return true;
}

static inline ulong get_current_cpu_hw_id()
{
    return fuzz::current_cpu;
}

static inline void irq_eoi()
{
}

static inline void irq_eoi(u8 vector)
{
    (void)vector;
}

static inline bool irq_is_in_service(u8 vector)
{
    (void)vector;
    return false;
}

static inline void send_ipi(ulong hw_id, u8 vector)
{
    (void)hw_id;
    (void)vector;
    fuzz::host_hal_unreachable("Hal::SendIpi");
}

static inline bool nmi_ipi_supported()
{
    return false;
}

static inline void send_nmi_ipi(ulong hw_id)
{
    (void)hw_id;
    fuzz::host_hal_unreachable("Hal::SendNmiIpi");
}

}
