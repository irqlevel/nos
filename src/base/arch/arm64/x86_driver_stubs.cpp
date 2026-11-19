#include <drivers/hpet.h>
#include <drivers/acpi.h>
#include <arch/x86_64/vtd.h>
#include <drivers/iommu.h>
#include <drivers/screen.h>
#include <hal/irq_stubs.h>

/* Link stubs for the x86-only paths still referenced from common code on
   arm64: ACPI and the HPET, which this architecture has neither of and does
   not build, and the x86 asm interrupt entry stubs a C++ driver's
   get_handler_fn returns (unused: arm64 dispatch is object-based). Every
   function here is unreachable on arm64.

   The virtio bus stubs that used to be here are gone with the C++ virtio
   drivers; the bus is Rust now (src/crates/drivers/virtio), and compiled for x86
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

/* No VT-d here: the shell's `iommu` says there is nothing to show (an SMMU
   would be this architecture's own) */
Vtd::Vtd()
    : unit_count_(0)
    , last_bus_(0)
    , on_(false)
    , remapping_(false)
    , io_apic_sid_(0)
    , io_apic_unit_(nullptr)
    , fault_apic_id_(0)
{
    for (size_t i = 0; i < stdlib::array_size(units_); i++)
        units_[i] = nullptr;
}

Vtd::~Vtd()
{
}

void Vtd::shell(const char* args, stdlib::Printer& printer)
{
    (void)args;
    printer.printf("no DMA remapping units\n");
}

/* drivers/iommu.h: no IOMMU on this architecture yet -- nothing translated,
   no domain to be had */
namespace iommu
{

bool on()
{
    return false;
}

ulong domain_create(u16 sid)
{
    (void)sid;
    return 0;
}

bool domain_map(ulong domain, u64 iova, u64 phys, ulong pages)
{
    (void)domain;
    (void)iova;
    (void)phys;
    (void)pages;
    return false;
}

bool domain_attach(ulong domain, u16 sid)
{
    (void)domain;
    (void)sid;
    return false;
}

bool domain_detach(ulong domain, u16 sid)
{
    (void)domain;
    (void)sid;
    return false;
}

void domain_destroy(ulong domain)
{
    (void)domain;
}

bool release(u16 sid)
{
    (void)sid;
    return false;
}

bool reserved(u16 sid, u64& base, u64& limit)
{
    (void)sid;
    (void)base;
    (void)limit;
    return false;
}

/* No interrupt remapping either: nothing is */
bool remapping()
{
    return false;
}

long remap_alloc(u16 sid, u8 vector, u32 apic_id, bool level)
{
    (void)sid;
    (void)vector;
    (void)apic_id;
    (void)level;
    return -1;
}

bool remap_retarget(u16 sid, u16 handle, u32 apic_id)
{
    (void)sid;
    (void)handle;
    (void)apic_id;
    return false;
}

void remap_free(u16 sid, u16 handle)
{
    (void)sid;
    (void)handle;
}

u16 io_apic_sid()
{
    return 0;
}

}

/* drivers/screen.h's hold and release: this architecture has no screen of
   the firmware's (drivers/screen.cpp is x86's) -- nothing to hold, none to
   let go of */
namespace screen
{

void hold(bool on)
{
    (void)on;
}

bool release_within(ulong base, ulong size)
{
    (void)base;
    (void)size;
    return false;
}

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

/* x86 asm interrupt entry points (hal/irq_stubs.h); get_handler_fn return
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
/* No entry point a slot: arm64's MSI-X are LPIs, dispatched by object */
void (*const rust_msix_stub_table[RUST_MSIX_SLOT_COUNT])() = {};
const unsigned long rust_msix_stub_count = RUST_MSIX_SLOT_COUNT;
}
