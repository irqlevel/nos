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

namespace Kernel
{

/* arm64 boots from a device tree, so drivers/acpi.cpp is not built here;
   these keep the rust_ffi ACPI query linkable. */
Acpi::Acpi()
    : Root(nullptr)
    , RootIsXsdt(false)
    , LapicAddress(nullptr)
    , IoApicAddress(nullptr)
    , IrqToGsiSize(0)
    , Pm1aCntPort(0)
    , ResetRegValid(false)
    , ResetRegPort(0)
    , ResetVal(0)
    , CenturyRegister(0)
    , HpetBasePhys(0)
    , HpetMinTick(0)
    , FirmwareWatchdog(false)
{
    OemId[0] = '\0';
}

Acpi::~Acpi()
{
}

bool Acpi::HasFirmwareWatchdog()
{
    return false;
}

Hpet::Hpet()
{
}

Hpet::~Hpet()
{
}

bool Hpet::IsAvailable()
{
    return false;
}

void Hpet::OnInterruptRegister(u8 irq, u8 vector)
{
    (void)irq;
    (void)vector;
}

InterruptHandlerFn Hpet::GetHandlerFn()
{
    return nullptr;
}

Stdlib::Time Hpet::GetTime()
{
    return Stdlib::Time();
}

}

/* x86 asm interrupt entry points (hal/irq_stubs.h); GetHandlerFn return
   values are ignored by the arm64 dispatch */
extern "C"
{
void IO8042InterruptStub() {}
void SerialInterruptStub() {}
void PitInterruptStub() {}
void HpetInterruptStub() {}
void IPInterruptStub() {}
void LapicTimerInterruptStub() {}
void VirtioNetInterruptStub() {}
void VirtioScsiInterruptStub() {}
void SharedInterruptStub() {}
void SpuriousInterruptStub() {}
void RustInterruptStub0() {}
void RustInterruptStub1() {}
void RustInterruptStub2() {}
void RustInterruptStub3() {}
void RustInterruptStub4() {}
void RustInterruptStub5() {}
void RustInterruptStub6() {}
void RustInterruptStub7() {}
void RustMsixStub0() {}
void RustMsixStub1() {}
void RustMsixStub2() {}
void RustMsixStub3() {}
void RustMsixStub4() {}
void RustMsixStub5() {}
void RustMsixStub6() {}
void RustMsixStub7() {}
void RustMsixStub8() {}
void RustMsixStub9() {}
void RustMsixStub10() {}
void RustMsixStub11() {}
void RustMsixStub12() {}
void RustMsixStub13() {}
void RustMsixStub14() {}
void RustMsixStub15() {}
void RustMsixStub16() {}
void RustMsixStub17() {}
void RustMsixStub18() {}
void RustMsixStub19() {}
void RustMsixStub20() {}
void RustMsixStub21() {}
void RustMsixStub22() {}
void RustMsixStub23() {}
void RustMsixStub24() {}
void RustMsixStub25() {}
void RustMsixStub26() {}
void RustMsixStub27() {}
void RustMsixStub28() {}
void RustMsixStub29() {}
void RustMsixStub30() {}
void RustMsixStub31() {}
}
