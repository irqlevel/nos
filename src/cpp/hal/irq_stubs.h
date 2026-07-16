#pragma once

// Portable interrupt entry-point contract. Every arch defines these symbols
// (x86_64: macro-generated stubs in arch/x86_64/asm.asm that push a Context
// frame and call the C++ handler; arm64 later: C veneers dispatched from the
// GIC IRQ path). Drivers hand them to Interrupt::Register via GetHandlerFn.
// The CPU-exception stubs (Exc*Stub) are arch-private and not part of this
// contract.

#ifdef __cplusplus
extern "C"
{
#endif

void IO8042InterruptStub();
void SerialInterruptStub();
void PitInterruptStub();
void HpetInterruptStub();
void IPInterruptStub();
void VirtioBlkInterruptStub();
void VirtioNetInterruptStub();
void VirtioScsiInterruptStub();
void SharedInterruptStub();

void DummyInterruptStub();

void SpuriousInterruptStub();

void RustInterruptStub0();
void RustInterruptStub1();
void RustInterruptStub2();
void RustInterruptStub3();
void RustInterruptStub4();
void RustInterruptStub5();
void RustInterruptStub6();
void RustInterruptStub7();

void RustMsixStub0();
void RustMsixStub1();
void RustMsixStub2();
void RustMsixStub3();
void RustMsixStub4();
void RustMsixStub5();
void RustMsixStub6();
void RustMsixStub7();
void RustMsixStub8();
void RustMsixStub9();
void RustMsixStub10();
void RustMsixStub11();
void RustMsixStub12();
void RustMsixStub13();
void RustMsixStub14();
void RustMsixStub15();
void RustMsixStub16();
void RustMsixStub17();
void RustMsixStub18();
void RustMsixStub19();
void RustMsixStub20();
void RustMsixStub21();
void RustMsixStub22();
void RustMsixStub23();
void RustMsixStub24();
void RustMsixStub25();
void RustMsixStub26();
void RustMsixStub27();
void RustMsixStub28();
void RustMsixStub29();
void RustMsixStub30();
void RustMsixStub31();

#ifdef __cplusplus
}
#endif
