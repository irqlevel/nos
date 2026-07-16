#pragma once

#include <include/types.h>

namespace Kernel
{

class InterruptHandler;

/* Device-facing virtio transport contract: everything a virtio device
   driver needs after the bus-specific probe found the device. Implemented
   by VirtioPci (legacy + modern virtio-pci); a virtio-mmio transport slots
   in behind the same interface. Probing stays bus-specific and is not part
   of the contract. */
class VirtioTransport
{
public:
    virtual void Reset() = 0;
    virtual u8   GetStatus() = 0;
    virtual void SetStatus(u8 s) = 0;

    virtual u32  ReadDeviceFeature(ulong index) = 0;
    virtual void WriteDriverFeature(ulong index, u32 val) = 0;

    virtual u16  GetNumQueues() = 0;
    virtual u8   ReadISR() = 0;
    virtual u8   ReadConfigGeneration() = 0;

    /* Per-queue operations (call SelectQueue first) */
    virtual void SelectQueue(u16 idx) = 0;
    virtual u16  GetQueueSize() = 0;
    virtual u16  GetQueueNotifyOff() = 0;
    virtual void SetQueueDesc(u64 physAddr) = 0;
    virtual void SetQueueDriver(u64 physAddr) = 0;
    virtual void SetQueueDevice(u64 physAddr) = 0;
    virtual void EnableQueue() = 0;

    /* Returns the MMIO address to write for notifying a queue.
       Transports without a per-queue doorbell address return nullptr;
       use NotifyQueue() instead. */
    virtual volatile void* GetNotifyAddr(u16 queueIdx) = 0;

    /* Notify a queue (always works) */
    virtual void NotifyQueue(u16 queueIdx) = 0;

    /* Device-specific config access */
    virtual u8   ReadDevCfg8(ulong offset) = 0;
    virtual u32  ReadDevCfg32(ulong offset) = 0;
    virtual u64  ReadDevCfg64(ulong offset) = 0;

    /* True if the legacy (transitional) virtio-pci transport is in use */
    virtual bool IsLegacy() const = 0;

    /* MSI-X (virtio-pci modern only; others return false/0) */
    virtual bool IsMsixEnabled() const = 0;
    virtual u8   EnableMsixVector(u16 index, InterruptHandler& handler) = 0;
    virtual bool UsingMsix() const = 0;

    /* Device status bits */
    static const u8 StatusAcknowledge = 1;
    static const u8 StatusDriver      = 2;
    static const u8 StatusDriverOk    = 4;
    static const u8 StatusFeaturesOk  = 8;
    static const u8 StatusFailed      = 128;

protected:
    /* Never deleted through the base */
    ~VirtioTransport() {}
};

}
