#pragma once

// Portable interrupt entry-point contract. Every arch defines these symbols
// (x86_64: macro-generated stubs in arch/x86_64/asm.asm that push a Context
// frame and call the C++ handler; arm64 later: C veneers dispatched from the
// GIC IRQ path). Drivers hand them to Interrupt::register_handler via get_handler_fn.
// The CPU-exception stubs (Exc*Stub) are arch-private and not part of this
// contract.

#ifdef __cplusplus
extern "C"
{
#endif

void io8042_interrupt_stub();
void serial_interrupt_stub();
void pit_interrupt_stub();
void hpet_interrupt_stub();
void ip_interrupt_stub();
void lapic_timer_interrupt_stub();
void posted_interrupt_stub();
void shared_interrupt_stub();

void spurious_interrupt_stub();

void rust_interrupt_stub0();
void rust_interrupt_stub1();
void rust_interrupt_stub2();
void rust_interrupt_stub3();
void rust_interrupt_stub4();
void rust_interrupt_stub5();
void rust_interrupt_stub6();
void rust_interrupt_stub7();

/* The Rust MSI-X slots' entry points, a slot's at its index: the arch's
   (x86_64: asm.asm's, generated to its RUST_MSIX_SLOTS; arm64: none used,
   its LPIs dispatch by object), and how many there are --
   RUST_MSIX_SLOT_COUNT, below, which the first registration holds it to */
extern void (*const rust_msix_stub_table[])();
extern const unsigned long rust_msix_stub_count;

#ifdef __cplusplus
}

/* How many Rust MSI-X slots there are (rust_ffi.cpp): a driver's interrupt
   each, a vector of a host function a guest holds each */
static const unsigned long RUST_MSIX_SLOT_COUNT = 64;
#endif
