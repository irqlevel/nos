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

void rust_msix_stub0();
void rust_msix_stub1();
void rust_msix_stub2();
void rust_msix_stub3();
void rust_msix_stub4();
void rust_msix_stub5();
void rust_msix_stub6();
void rust_msix_stub7();
void rust_msix_stub8();
void rust_msix_stub9();
void rust_msix_stub10();
void rust_msix_stub11();
void rust_msix_stub12();
void rust_msix_stub13();
void rust_msix_stub14();
void rust_msix_stub15();
void rust_msix_stub16();
void rust_msix_stub17();
void rust_msix_stub18();
void rust_msix_stub19();
void rust_msix_stub20();
void rust_msix_stub21();
void rust_msix_stub22();
void rust_msix_stub23();
void rust_msix_stub24();
void rust_msix_stub25();
void rust_msix_stub26();
void rust_msix_stub27();
void rust_msix_stub28();
void rust_msix_stub29();
void rust_msix_stub30();
void rust_msix_stub31();

#ifdef __cplusplus
}
#endif
