#include <drivers/pci.h>
#include <drivers/virtio_pci.h>
#include <drivers/virtio_scsi.h>
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

void VirtioScsi::DrainAllQueues()
{
    /* No scsi instances on arm64 yet */
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
}
