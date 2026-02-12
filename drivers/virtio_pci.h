#pragma once

#include <include/types.h>
#include <drivers/pci.h>

namespace Kernel
{

class VirtioPci
{
public:
    VirtioPci();
    ~VirtioPci();

    bool Probe(Pci::DeviceInfo* dev);

    void Reset();
    u8   GetStatus();
    void SetStatus(u8 s);

    u32  ReadDeviceFeature(ulong index);
    void WriteDriverFeature(ulong index, u32 val);

    u16  GetNumQueues();
    u8   ReadISR();
    u8   ReadConfigGeneration();

    /* Per-queue operations (call SelectQueue first) */
    void SelectQueue(u16 idx);
    u16  GetQueueSize();
    u16  GetQueueNotifyOff();
    void SetQueueDesc(u64 physAddr);
    void SetQueueDriver(u64 physAddr);
    void SetQueueDevice(u64 physAddr);
    void EnableQueue();

    /* Returns the MMIO address to write for notifying a queue.
       For legacy mode returns nullptr; use NotifyQueue() instead. */
    volatile void* GetNotifyAddr(u16 queueIdx);

    /* Notify a queue (works for both legacy and modern) */
    void NotifyQueue(u16 queueIdx);

    /* Device-specific config access */
    u8   ReadDevCfg8(ulong offset);
    u32  ReadDevCfg32(ulong offset);
    u64  ReadDevCfg64(ulong offset);

    /* True if legacy (transitional) transport is in use */
    bool IsLegacy() const { return Legacy; }

    /* Virtio PCI capability cfg_type values */
    static const u8 CapCommonCfg  = 1;
    static const u8 CapNotifyCfg  = 2;
    static const u8 CapIsrCfg     = 3;
    static const u8 CapDeviceCfg  = 4;
    static const u8 CapPciCfg     = 5;

    /* PCI capability ID for vendor-specific */
    static const u8 PciCapIdVndr  = 0x09;

    /* Device status bits */
    static const u8 StatusAcknowledge = 1;
    static const u8 StatusDriver      = 2;
    static const u8 StatusDriverOk    = 4;
    static const u8 StatusFeaturesOk  = 8;
    static const u8 StatusFailed      = 128;

private:
    VirtioPci(const VirtioPci& other) = delete;
    VirtioPci(VirtioPci&& other) = delete;
    VirtioPci& operator=(const VirtioPci& other) = delete;
    VirtioPci& operator=(VirtioPci&& other) = delete;

    bool ProbeModern(Pci::DeviceInfo* dev);
    bool ProbeLegacy(Pci::DeviceInfo* dev);

    /* Map a BAR and return the kernel virtual base.
       Caches results so the same BAR is only mapped once. */
    ulong MapBar(Pci::DeviceInfo* dev, u8 bar);

    bool Legacy;
    u16  IoBase;     /* BAR0 I/O port base for legacy transport */

    volatile u8* CommonCfg;
    volatile u8* NotifyBase;
    u32 NotifyOffMultiplier;
    volatile u8* IsrCfg;
    volatile u8* DeviceCfg;

    /* Cached per-queue notify addresses (modern only) */
    static const ulong MaxCachedQueues = 4;
    volatile u8* NotifyAddr[MaxCachedQueues];

    /* Cached mapped BAR virtual addresses */
    static const ulong MaxBars = 6;
    ulong MappedBars[MaxBars];

    /* Modern common config register offsets */
    static const ulong CfgDeviceFeatureSelect = 0x00;
    static const ulong CfgDeviceFeature       = 0x04;
    static const ulong CfgDriverFeatureSelect = 0x08;
    static const ulong CfgDriverFeature       = 0x0C;
    static const ulong CfgMsixConfig          = 0x10;
    static const ulong CfgNumQueues           = 0x12;
    static const ulong CfgDeviceStatus        = 0x14;
    static const ulong CfgConfigGeneration    = 0x15;
    static const ulong CfgQueueSelect         = 0x16;
    static const ulong CfgQueueSize           = 0x18;
    static const ulong CfgQueueMsixVector     = 0x1A;
    static const ulong CfgQueueEnable         = 0x1C;
    static const ulong CfgQueueNotifyOff      = 0x1E;
    static const ulong CfgQueueDesc           = 0x20;
    static const ulong CfgQueueDriver         = 0x28;
    static const ulong CfgQueueDevice         = 0x30;

    /* Legacy I/O port register offsets (relative to IoBase) */
    static const u16 LegDeviceFeatures  = 0x00;
    static const u16 LegDriverFeatures  = 0x04;
    static const u16 LegQueueAddress    = 0x08;
    static const u16 LegQueueSize       = 0x0C;
    static const u16 LegQueueSelect     = 0x0E;
    static const u16 LegQueueNotify     = 0x10;
    static const u16 LegDeviceStatus    = 0x12;
    static const u16 LegISRStatus       = 0x13;
    static const u16 LegDeviceConfig    = 0x14;
};

}
