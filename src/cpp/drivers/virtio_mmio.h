#pragma once

#include <include/types.h>
#include <drivers/virtio_transport.h>

namespace Kernel
{

/* A discovered virtio-mmio device slot (built from the device tree on
   arm64; the transport itself is arch-neutral). */
struct VirtioMmioSlot
{
    ulong Base;   /* kernel VA of the register window */
    ulong Size;
    u32 IntId;
    u32 DeviceId; /* virtio device id: 1 net, 2 blk, 4 rng, 8 scsi */
};

/* Modern (version 2) virtio-mmio transport. Legacy (v1) is deliberately
   unsupported: run scripts pass -global virtio-mmio.force-legacy=false. */
class VirtioMmio final : public VirtioTransport
{
public:
    VirtioMmio();
    ~VirtioMmio();

    /* base is a mapped VA; returns false unless magic/version match */
    bool Probe(ulong base, ulong size);

    static u32 ReadDeviceId(ulong base);

    void Reset() override;
    u8   GetStatus() override;
    void SetStatus(u8 s) override;

    u32  ReadDeviceFeature(ulong index) override;
    void WriteDriverFeature(ulong index, u32 val) override;

    u16  GetNumQueues() override;
    u8   ReadISR() override;
    u8   ReadConfigGeneration() override;

    void SelectQueue(u16 idx) override;
    u16  GetQueueSize() override;
    u16  GetQueueNotifyOff() override;
    void SetQueueDesc(u64 physAddr) override;
    void SetQueueDriver(u64 physAddr) override;
    void SetQueueDevice(u64 physAddr) override;
    void EnableQueue() override;

    volatile void* GetNotifyAddr(u16 queueIdx) override;
    void NotifyQueue(u16 queueIdx) override;

    u8   ReadDevCfg8(ulong offset) override;
    u32  ReadDevCfg32(ulong offset) override;
    u64  ReadDevCfg64(ulong offset) override;

    bool IsLegacy() const override { return false; }

    bool IsMsixEnabled() const override { return false; }
    u8   EnableMsixVector(u16 index, InterruptHandler& handler) override;
    bool UsingMsix() const override { return false; }

private:
    VirtioMmio(const VirtioMmio& other) = delete;
    VirtioMmio(VirtioMmio&& other) = delete;
    VirtioMmio& operator=(const VirtioMmio& other) = delete;
    VirtioMmio& operator=(VirtioMmio&& other) = delete;

    ulong Base;
    u16 CurQueueSize;

    static const u32 MagicValue = 0x74726976;

    static const ulong RegMagic = 0x000;
    static const ulong RegVersion = 0x004;
    static const ulong RegDeviceId = 0x008;
    static const ulong RegDeviceFeatures = 0x010;
    static const ulong RegDeviceFeaturesSel = 0x014;
    static const ulong RegDriverFeatures = 0x020;
    static const ulong RegDriverFeaturesSel = 0x024;
    static const ulong RegQueueSel = 0x030;
    static const ulong RegQueueNumMax = 0x034;
    static const ulong RegQueueNum = 0x038;
    static const ulong RegQueueReady = 0x044;
    static const ulong RegQueueNotify = 0x050;
    static const ulong RegInterruptStatus = 0x060;
    static const ulong RegInterruptAck = 0x064;
    static const ulong RegStatus = 0x070;
    static const ulong RegQueueDescLow = 0x080;
    static const ulong RegQueueDescHigh = 0x084;
    static const ulong RegQueueDriverLow = 0x090;
    static const ulong RegQueueDriverHigh = 0x094;
    static const ulong RegQueueDeviceLow = 0x0A0;
    static const ulong RegQueueDeviceHigh = 0x0A4;
    static const ulong RegConfigGeneration = 0x0FC;
    static const ulong RegConfig = 0x100;
};

}
