#include <drivers/hpet.h>
#include <drivers/acpi.h>
#include <hal/irq_stubs.h>

/* Link stubs for the x86-only paths still referenced from common code on
   arm64: ACPI and the HPET, which this architecture has neither of and does
   not build, and the x86 asm interrupt entry stubs a C++ driver's
   GetHandlerFn returns (unused: arm64 dispatch is object-based). Every
   function here is unreachable on arm64.

   The virtio bus stubs that used to be here are gone with the C++ virtio
   drivers; the bus is Rust now (src/rust/virtio), and compiled for x86
   only. */

namespace kernel
{

/* arm64 boots from a device tree, so drivers/acpi.cpp is not built here;
   these keep the rust_ffi ACPI query linkable. */
Acpi::Acpi()
    : root_(nullptr)
    , root_is_xsdt_(false)
    , lapic_address_(nullptr)
    , io_apic_address_(nullptr)
    , irq_to_gsi_size_(0)
    , pm1a_cnt_port_(0)
    , reset_reg_valid_(false)
    , reset_reg_port_(0)
    , reset_val_(0)
    , century_register_(0)
    , hpet_base_phys_(0)
    , hpet_min_tick_(0)
    , firmware_watchdog_(false)
{
    oem_id_[0] = '\0';
}

Acpi::~Acpi()
{
}

bool Acpi::has_firmware_watchdog()
{
    return false;
}

Hpet::Hpet()
{
}

Hpet::~Hpet()
{
}

bool Hpet::is_available()
{
    return false;
}

void Hpet::on_interrupt_register(u8 irq, u8 vector)
{
    (void)irq;
    (void)vector;
}

InterruptHandlerFn Hpet::get_handler_fn()
{
    return nullptr;
}

stdlib::Time Hpet::get_time()
{
    return stdlib::Time();
}

}

/* x86 asm interrupt entry points (hal/irq_stubs.h); GetHandlerFn return
   values are ignored by the arm64 dispatch */
extern "C"
{
void io8042_interrupt_stub() {}
void serial_interrupt_stub() {}
void pit_interrupt_stub() {}
void hpet_interrupt_stub() {}
void ip_interrupt_stub() {}
void lapic_timer_interrupt_stub() {}
void posted_interrupt_stub() {}
void virtio_net_interrupt_stub() {}
void virtio_scsi_interrupt_stub() {}
void shared_interrupt_stub() {}
void spurious_interrupt_stub() {}
void rust_interrupt_stub0() {}
void rust_interrupt_stub1() {}
void rust_interrupt_stub2() {}
void rust_interrupt_stub3() {}
void rust_interrupt_stub4() {}
void rust_interrupt_stub5() {}
void rust_interrupt_stub6() {}
void rust_interrupt_stub7() {}
void rust_msix_stub0() {}
void rust_msix_stub1() {}
void rust_msix_stub2() {}
void rust_msix_stub3() {}
void rust_msix_stub4() {}
void rust_msix_stub5() {}
void rust_msix_stub6() {}
void rust_msix_stub7() {}
void rust_msix_stub8() {}
void rust_msix_stub9() {}
void rust_msix_stub10() {}
void rust_msix_stub11() {}
void rust_msix_stub12() {}
void rust_msix_stub13() {}
void rust_msix_stub14() {}
void rust_msix_stub15() {}
void rust_msix_stub16() {}
void rust_msix_stub17() {}
void rust_msix_stub18() {}
void rust_msix_stub19() {}
void rust_msix_stub20() {}
void rust_msix_stub21() {}
void rust_msix_stub22() {}
void rust_msix_stub23() {}
void rust_msix_stub24() {}
void rust_msix_stub25() {}
void rust_msix_stub26() {}
void rust_msix_stub27() {}
void rust_msix_stub28() {}
void rust_msix_stub29() {}
void rust_msix_stub30() {}
void rust_msix_stub31() {}
}
