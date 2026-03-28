#include "virtio_rng.h"

#include <kernel/trace.h>
#include <kernel/asm.h>
#include <lib/stdlib.h>
#include <mm/new.h>
#include <include/const.h>

namespace Kernel
{

VirtioRng VirtioRng::Instances[MaxInstances];
ulong VirtioRng::InstanceCount = 0;

VirtioRng::VirtioRng()
    : DmaBuf(nullptr)
    , DmaBufPhys(0)
    , Initialized(false)
{
    DevName[0] = '\0';
}

VirtioRng::~VirtioRng()
{
}

bool VirtioRng::Init(Pci::DeviceInfo* pciDev, const char* name)
{
    auto& pci = Pci::GetInstance();

    ulong nameLen = Stdlib::StrLen(name);
    if (nameLen >= sizeof(DevName))
        nameLen = sizeof(DevName) - 1;
    Stdlib::MemCpy(DevName, name, nameLen);
    DevName[nameLen] = '\0';

    /* Enable PCI bus mastering */
    pci.EnableBusMastering(pciDev->Bus, pciDev->Slot, pciDev->Func);

    /* Probe modern virtio-pci capabilities and map MMIO BARs */
    if (!Transport.Probe(pciDev))
    {
        Trace(0, "VirtioRng %s: Transport.Probe failed", name);
        return false;
    }

    Trace(0, "VirtioRng %s: %s virtio-pci probed, irq %u",
        name, Transport.IsLegacy() ? "legacy" : "modern",
        (ulong)pciDev->InterruptLine);

    /* Reset device */
    Transport.Reset();

    /* Acknowledge */
    Transport.SetStatus(VirtioPci::StatusAcknowledge);

    /* Driver */
    Transport.SetStatus(VirtioPci::StatusAcknowledge | VirtioPci::StatusDriver);

    /* Read and negotiate features -- virtio-rng has no device-specific features */
    u32 devFeatures0 = Transport.ReadDeviceFeature(0);
    Trace(0, "VirtioRng %s: device features[0] 0x%p", name, (ulong)devFeatures0);

    Transport.WriteDriverFeature(0, 0);

    if (!Transport.IsLegacy())
    {
        /* features[1]: set VIRTIO_F_VERSION_1 (bit 32 = index 1 bit 0) */
        u32 devFeatures1 = Transport.ReadDeviceFeature(1);
        u32 drvFeatures1 = devFeatures1 & (1 << 0); /* VIRTIO_F_VERSION_1 */
        Transport.WriteDriverFeature(1, drvFeatures1);
    }

    if (!Transport.IsLegacy())
    {
        /* Set FEATURES_OK (modern only) */
        Transport.SetStatus(VirtioPci::StatusAcknowledge | VirtioPci::StatusDriver |
                            VirtioPci::StatusFeaturesOk);

        /* Verify FEATURES_OK is still set */
        if (!(Transport.GetStatus() & VirtioPci::StatusFeaturesOk))
        {
            Trace(0, "VirtioRng %s: FEATURES_OK not set by device", name);
            Transport.SetStatus(VirtioPci::StatusFailed);
            return false;
        }
    }

    /* Setup requestq (queue 0) */
    Transport.SelectQueue(0);
    u16 queueSize = Transport.GetQueueSize();
    Trace(0, "VirtioRng %s: queue size %u", name, (ulong)queueSize);

    if (queueSize == 0)
    {
        Trace(0, "VirtioRng %s: queue size is 0", name);
        Transport.SetStatus(VirtioPci::StatusFailed);
        return false;
    }

    if (!Queue.Setup(queueSize))
    {
        Trace(0, "VirtioRng %s: failed to setup queue", name);
        Transport.SetStatus(VirtioPci::StatusFailed);
        return false;
    }

    Transport.SetQueueDesc(Queue.GetDescPhys());
    Transport.SetQueueDriver(Queue.GetAvailPhys());
    Transport.SetQueueDevice(Queue.GetUsedPhys());
    Transport.EnableQueue();

    /* Set DRIVER_OK */
    u8 okStatus = VirtioPci::StatusAcknowledge | VirtioPci::StatusDriver |
                  VirtioPci::StatusDriverOk;
    if (!Transport.IsLegacy())
        okStatus |= VirtioPci::StatusFeaturesOk;
    Transport.SetStatus(okStatus);

    /* Allocate DMA buffer (1 page) */
    DmaBuf = (u8*)Mm::AllocMapPages(1, &DmaBufPhys);
    if (!DmaBuf)
    {
        Trace(0, "VirtioRng %s: failed to alloc DMA page", name);
        Transport.SetStatus(VirtioPci::StatusFailed);
        return false;
    }

    Initialized = true;
    Trace(0, "VirtioRng %s: initialized", name);
    return true;
}

const char* VirtioRng::GetName()
{
    return DevName;
}

bool VirtioRng::GetRandom(u8* buf, ulong len)
{
    if (!Initialized || len == 0)
        return false;

    Stdlib::AutoLock lock(Lock);

    ulong offset = 0;
    while (offset < len)
    {
        ulong chunkSize = len - offset;
        if (chunkSize > Const::PageSize)
            chunkSize = Const::PageSize;

        /* Post a device-writable buffer */
        VirtQueue::BufDesc desc;
        desc.Addr = DmaBufPhys;
        desc.Len = (u32)chunkSize;
        desc.Writable = true;

        int head = Queue.AddBufs(&desc, 1);
        if (head < 0)
        {
            Trace(0, "VirtioRng %s: AddBufs failed", DevName);
            return false;
        }

        Transport.NotifyQueue(0);

        /* Poll for completion */
        for (ulong i = 0; i < 10000000; i++)
        {
            if (Queue.HasUsed())
                break;
            Pause();
        }

        u32 usedId, usedLen;
        if (!Queue.GetUsed(usedId, usedLen))
        {
            Trace(0, "VirtioRng %s: timeout", DevName);
            return false;
        }

        /* Copy returned random bytes */
        ulong got = usedLen;
        if (got > chunkSize)
            got = chunkSize;

        Stdlib::MemCpy(buf + offset, DmaBuf, got);
        offset += got;

        /* Device may return fewer bytes than requested */
        if (got == 0)
        {
            Trace(0, "VirtioRng %s: device returned 0 bytes", DevName);
            return false;
        }
    }

    return true;
}

void VirtioRng::InitAll()
{
    auto& pci = Pci::GetInstance();
    InstanceCount = 0;

    for (ulong i = 0; i < MaxInstances; i++)
        new (&Instances[i]) VirtioRng();

    for (ulong i = 0; i < pci.GetDeviceCount() && InstanceCount < MaxInstances; i++)
    {
        Pci::DeviceInfo* dev = pci.GetDevice(i);
        if (!dev)
            break;

        if (dev->Vendor != Pci::VendorVirtio)
            continue;
        if (dev->Device != Pci::DevVirtioRng && dev->Device != Pci::DevVirtioRngModern)
            continue;

        char name[8];
        name[0] = 'r';
        name[1] = 'n';
        name[2] = 'g';
        name[3] = (char)('0' + InstanceCount);
        name[4] = '\0';

        VirtioRng& inst = Instances[InstanceCount];
        if (inst.Init(dev, name))
        {
            EntropySourceTable::GetInstance().Register(&inst);
            InstanceCount++;
        }
    }

    Trace(0, "VirtioRng: initialized %u devices", InstanceCount);
}

}
