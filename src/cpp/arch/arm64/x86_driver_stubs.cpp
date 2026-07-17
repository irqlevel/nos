#include <drivers/pci.h>
#include <drivers/virtio_pci.h>
#include <drivers/virtio_scsi.h>
#include <drivers/msix.h>
#include <drivers/hpet.h>
#include <hal/irq_stubs.h>

/* Link stubs for the x86-only device paths still referenced from common
   driver code on arm64:
   - the PCI Init(...) overloads of the virtio drivers (never called: PCI
     enumeration does not exist here until the ECAM work lands),
   - the x86 asm interrupt entry stubs returned by GetHandlerFn (unused:
     arm64 dispatch is object-based),
   - VirtioScsi (not yet ported to the mmio transport).
   Every function here is unreachable on arm64. */

namespace Kernel
{

namespace
{

void __attribute__((noreturn)) StubTrap()
{
    /* Reaching any of these means a PCI-only path ran on arm64 */
    for (;;)
    {
        asm volatile("brk #0");
    }
}

}

MsixTable::MsixTable()
{
}

MsixTable::~MsixTable()
{
}

bool MsixTable::Setup(Pci::DeviceInfo* dev, const ulong* mappedBars)
{
    (void)dev;
    (void)mappedBars;
    return false;
}

u8 MsixTable::EnableVector(u16 index, InterruptHandler& handler)
{
    (void)index;
    (void)handler;
    return 0;
}

void MsixTable::Mask(u16 index)
{
    (void)index;
}

void MsixTable::Unmask(u16 index)
{
    (void)index;
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

VirtioPci::VirtioPci()
    : Legacy(false)
    , IoBase(0)
    , CommonCfg(nullptr)
    , NotifyBase(nullptr)
    , NotifyOffMultiplier(0)
    , IsrCfg(nullptr)
    , DeviceCfg(nullptr)
    , PciDev(nullptr)
    , MsixActive(false)
    , MsixEntry(0)
{
}

VirtioPci::~VirtioPci()
{
}

bool VirtioPci::Probe(Pci::DeviceInfo* dev)
{
    (void)dev;
    return false;
}

void VirtioPci::Reset() { StubTrap(); }
u8   VirtioPci::GetStatus() { StubTrap(); }
void VirtioPci::SetStatus(u8 s) { (void)s; StubTrap(); }
u32  VirtioPci::ReadDeviceFeature(ulong index) { (void)index; StubTrap(); }
void VirtioPci::WriteDriverFeature(ulong index, u32 val) { (void)index; (void)val; StubTrap(); }
u16  VirtioPci::GetNumQueues() { StubTrap(); }
u8   VirtioPci::ReadISR() { StubTrap(); }
u8   VirtioPci::ReadConfigGeneration() { StubTrap(); }
void VirtioPci::SelectQueue(u16 idx) { (void)idx; StubTrap(); }
u16  VirtioPci::GetQueueSize() { StubTrap(); }
u16  VirtioPci::GetQueueNotifyOff() { StubTrap(); }
void VirtioPci::SetQueueDesc(u64 physAddr) { (void)physAddr; StubTrap(); }
void VirtioPci::SetQueueDriver(u64 physAddr) { (void)physAddr; StubTrap(); }
void VirtioPci::SetQueueDevice(u64 physAddr) { (void)physAddr; StubTrap(); }
void VirtioPci::EnableQueue() { StubTrap(); }
volatile void* VirtioPci::GetNotifyAddr(u16 queueIdx) { (void)queueIdx; StubTrap(); }
void VirtioPci::NotifyQueue(u16 queueIdx) { (void)queueIdx; StubTrap(); }
u8   VirtioPci::ReadDevCfg8(ulong offset) { (void)offset; StubTrap(); }
u32  VirtioPci::ReadDevCfg32(ulong offset) { (void)offset; StubTrap(); }
u64  VirtioPci::ReadDevCfg64(ulong offset) { (void)offset; StubTrap(); }
u8   VirtioPci::EnableMsixVector(u16 index, InterruptHandler& handler)
{
    (void)index;
    (void)handler;
    return 0;
}

}

/* Pci methods beyond the Dump stub (see pci_stub.cpp) */
void Pci::EnableBusMastering(u16 bus, u16 slot, u16 func)
{
    (void)bus;
    (void)slot;
    (void)func;
}

Pci::DeviceInfo* Pci::GetDevice(ulong index)
{
    (void)index;
    return nullptr;
}

ulong Pci::GetDeviceCount()
{
    return 0;
}

u8 Pci::ReadByte(u16 bus, u16 slot, u16 func, u16 offset)
{
    (void)bus; (void)slot; (void)func; (void)offset;
    return 0xFF;
}

u16 Pci::ReadWord(u16 bus, u16 slot, u16 func, u16 offset)
{
    (void)bus; (void)slot; (void)func; (void)offset;
    return 0xFFFF;
}

u32 Pci::ReadDword(u16 bus, u16 slot, u16 func, u16 offset)
{
    (void)bus; (void)slot; (void)func; (void)offset;
    return 0xFFFFFFFF;
}

void Pci::WriteByte(u16 bus, u16 slot, u16 func, u16 offset, u8 value)
{
    (void)bus; (void)slot; (void)func; (void)offset; (void)value;
}

void Pci::WriteWord(u16 bus, u16 slot, u16 func, u16 offset, u16 value)
{
    (void)bus; (void)slot; (void)func; (void)offset; (void)value;
}

void Pci::WriteDword(u16 bus, u16 slot, u16 func, u16 offset, u32 value)
{
    (void)bus; (void)slot; (void)func; (void)offset; (void)value;
}

u8 Pci::FindCapability(u16 bus, u16 slot, u16 func, u8 capId, u8 startOffset)
{
    (void)bus; (void)slot; (void)func; (void)capId; (void)startOffset;
    return 0;
}

u32 Pci::GetBAR(u16 bus, u16 slot, u16 func, u8 bar)
{
    (void)bus; (void)slot; (void)func; (void)bar;
    return 0;
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
void VirtioBlkInterruptStub() {}
void VirtioNetInterruptStub() {}
void VirtioScsiInterruptStub() {}
void SharedInterruptStub() {}
void DummyInterruptStub() {}
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
