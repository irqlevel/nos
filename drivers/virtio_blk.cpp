#include "virtio_blk.h"
#include "lapic.h"
#include "ioapic.h"
#include "acpi.h"

#include <kernel/trace.h>
#include <kernel/asm.h>
#include <kernel/panic.h>
#include <kernel/interrupt.h>
#include <kernel/idt.h>
#include <mm/page_table.h>
#include <mm/memory_map.h>
#include <mm/new.h>

namespace Kernel
{

VirtioBlk VirtioBlk::Instances[MaxInstances];
ulong VirtioBlk::InstanceCount = 0;

VirtioBlk::VirtioBlk()
    : QueueNotifyAddr(nullptr)
    , CapacitySectors(0)
    , IntVector(-1)
    , Initialized(false)
    , ReqHeader(nullptr)
    , ReqHeaderPhys(0)
    , DataBuf(nullptr)
    , DataBufPhys(0)
    , StatusBuf(nullptr)
    , StatusBufPhys(0)
{
    DevName[0] = '\0';
}

VirtioBlk::~VirtioBlk()
{
}

bool VirtioBlk::Init(Pci::DeviceInfo* pciDev, const char* name)
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
        Trace(0, "VirtioBlk %s: Transport.Probe failed", name);
        return false;
    }

    Trace(0, "VirtioBlk %s: %s virtio-pci probed, irq %u",
        name, Transport.IsLegacy() ? "legacy" : "modern",
        (ulong)pciDev->InterruptLine);

    /* Reset device */
    Transport.Reset();

    /* Acknowledge */
    Transport.SetStatus(VirtioPci::StatusAcknowledge);

    /* Driver */
    Transport.SetStatus(VirtioPci::StatusAcknowledge | VirtioPci::StatusDriver);

    /* Read and negotiate features (64-bit via select) */
    u32 devFeatures0 = Transport.ReadDeviceFeature(0);
    Trace(0, "VirtioBlk %s: device features[0] 0x%p", name, (ulong)devFeatures0);

    /* We don't need any special features for basic operation. */
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
        /* Set FEATURES_OK (modern only; legacy doesn't have this bit) */
        Transport.SetStatus(VirtioPci::StatusAcknowledge | VirtioPci::StatusDriver |
                            VirtioPci::StatusFeaturesOk);

        /* Verify FEATURES_OK is still set */
        if (!(Transport.GetStatus() & VirtioPci::StatusFeaturesOk))
        {
            Trace(0, "VirtioBlk %s: FEATURES_OK not set by device", name);
            Transport.SetStatus(VirtioPci::StatusFailed);
            return false;
        }
    }

    /* Setup virtqueue 0 (request queue) */
    Transport.SelectQueue(0);
    u16 queueSize = Transport.GetQueueSize();
    Trace(0, "VirtioBlk %s: queue size %u", name, (ulong)queueSize);

    if (queueSize == 0)
    {
        Trace(0, "VirtioBlk %s: queue size is 0", name);
        Transport.SetStatus(VirtioPci::StatusFailed);
        return false;
    }

    if (!Queue.Setup(queueSize))
    {
        Trace(0, "VirtioBlk %s: failed to setup queue", name);
        Transport.SetStatus(VirtioPci::StatusFailed);
        return false;
    }

    Transport.SetQueueDesc(Queue.GetDescPhys());
    Transport.SetQueueDriver(Queue.GetAvailPhys());
    Transport.SetQueueDevice(Queue.GetUsedPhys());
    Transport.EnableQueue();

    if (!Transport.IsLegacy())
        QueueNotifyAddr = Transport.GetNotifyAddr(0);

    /* Set DRIVER_OK */
    u8 okStatus = VirtioPci::StatusAcknowledge | VirtioPci::StatusDriver |
                  VirtioPci::StatusDriverOk;
    if (!Transport.IsLegacy())
        okStatus |= VirtioPci::StatusFeaturesOk;
    Transport.SetStatus(okStatus);

    /* Read device config: capacity (u64 at offset 0) */
    CapacitySectors = Transport.ReadDevCfg64(0);

    Trace(0, "VirtioBlk %s: capacity %u sectors (%u MB)",
        name, CapacitySectors, (CapacitySectors * 512) / (1024 * 1024));

    /* Allocate pages for DMA buffers (request header, data, status). */
    auto& pt = Mm::PageTable::GetInstance();
    Mm::Page* dmaPage = pt.AllocContiguousPages(2);
    if (!dmaPage)
    {
        Trace(0, "VirtioBlk %s: failed to alloc DMA pages", name);
        Transport.SetStatus(VirtioPci::StatusFailed);
        return false;
    }

    ulong dmaPhys = dmaPage->GetPhyAddress();

    /* Map the DMA pages into the virtual address space. */
    for (ulong i = 0; i < 2; i++)
    {
        ulong va = dmaPage[i].GetPhyAddress() + Mm::MemoryMap::KernelSpaceBase;
        if (!pt.MapPage(va, &dmaPage[i]))
        {
            Trace(0, "VirtioBlk %s: failed to map DMA page %u", name, i);
            Transport.SetStatus(VirtioPci::StatusFailed);
            return false;
        }
    }

    ulong dmaVirt = dmaPhys + Mm::MemoryMap::KernelSpaceBase;

    /* Layout within the 2 DMA pages:
       Offset 0:   VirtioBlkReq header (16 bytes)
       Offset 16:  status byte (1 byte)
       Offset 4096: 512-byte data buffer (on page boundary for alignment) */
    ReqHeader = (VirtioBlkReq*)dmaVirt;
    ReqHeaderPhys = dmaPhys;
    StatusBuf = (u8*)(dmaVirt + sizeof(VirtioBlkReq));
    StatusBufPhys = dmaPhys + sizeof(VirtioBlkReq);
    DataBuf = (u8*)(dmaVirt + Const::PageSize);
    DataBufPhys = dmaPhys + Const::PageSize;

    Initialized = true;

    /* Register IRQ handler.  Use vector 0x25 + instance offset. */
    u8 irq = pciDev->InterruptLine;
    u8 vector = 0x25 + (u8)InstanceCount;
    auto& acpi = Acpi::GetInstance();
    Interrupt::RegisterLevel(*this, acpi.GetGsiByIrq(irq), vector);

    /* Register as block device */
    BlockDeviceTable::GetInstance().Register(this);

    Trace(0, "VirtioBlk %s: initialized", name);
    return true;
}

const char* VirtioBlk::GetName()
{
    return DevName;
}

u64 VirtioBlk::GetCapacity()
{
    return CapacitySectors;
}

u64 VirtioBlk::GetSectorSize()
{
    return 512;
}

bool VirtioBlk::DoIO(u32 type, u64 sector, void* buf)
{
    if (!Initialized)
        return false;

    Stdlib::AutoLock lock(IoLock);

    /* Build the request header */
    ReqHeader->Type = type;
    ReqHeader->Reserved = 0;
    ReqHeader->Sector = sector;
    *StatusBuf = 0xFF; /* Will be overwritten by device */

    /* Copy data for write */
    if (type == TypeOut)
    {
        Stdlib::MemCpy(DataBuf, buf, 512);
    }
    else
    {
        Stdlib::MemSet(DataBuf, 0, 512);
    }

    Barrier();

    /* Build 3-descriptor chain: header, data, status */
    VirtQueue::BufDesc bufs[3];

    /* Descriptor 0: request header (device-readable) */
    bufs[0].Addr = ReqHeaderPhys;
    bufs[0].Len = sizeof(VirtioBlkReq);
    bufs[0].Writable = false;

    /* Descriptor 1: data buffer */
    bufs[1].Addr = DataBufPhys;
    bufs[1].Len = 512;
    bufs[1].Writable = (type == TypeIn); /* Writable for reads */

    /* Descriptor 2: status byte (device-writable) */
    bufs[2].Addr = StatusBufPhys;
    bufs[2].Len = 1;
    bufs[2].Writable = true;

    int head = Queue.AddBufs(bufs, 3);
    if (head < 0)
    {
        Trace(0, "VirtioBlk %s: AddBufs failed", DevName);
        return false;
    }

    /* Kick the device */
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
        Trace(0, "VirtioBlk %s: timeout waiting for IO", DevName);
        return false;
    }

    if (*StatusBuf != 0)
    {
        Trace(0, "VirtioBlk %s: IO error status %u", DevName, (ulong)*StatusBuf);
        return false;
    }

    /* Copy data for read */
    if (type == TypeIn)
    {
        Stdlib::MemCpy(buf, DataBuf, 512);
    }

    return true;
}

bool VirtioBlk::ReadSector(u64 sector, void* buf)
{
    return DoIO(TypeIn, sector, buf);
}

bool VirtioBlk::WriteSector(u64 sector, const void* buf)
{
    return DoIO(TypeOut, sector, (void*)buf);
}

void VirtioBlk::OnInterruptRegister(u8 irq, u8 vector)
{
    (void)irq;
    IntVector = vector;
    Trace(0, "VirtioBlk %s: interrupt registered vector 0x%p", DevName, (ulong)vector);
}

InterruptHandlerFn VirtioBlk::GetHandlerFn()
{
    return VirtioBlkInterruptStub;
}

void VirtioBlk::OnInterrupt(Context* ctx)
{
    /* Called by shared interrupt dispatch (no EOI here). */
    Interrupt(ctx);
}

void VirtioBlk::Interrupt(Context* ctx)
{
    (void)ctx;
    InterruptCounter.Inc();

    /* Read ISR status to acknowledge interrupt */
    Transport.ReadISR();
}

void VirtioBlk::InitAll()
{
    auto& pci = Pci::GetInstance();
    InstanceCount = 0;

    /* Global constructors are not called in this kernel (no .init_array
       in the linker script), so BSS-resident objects lack vtable setup.
       Explicitly construct each instance via placement new. */
    for (ulong i = 0; i < MaxInstances; i++)
        new (&Instances[i]) VirtioBlk();

    for (ulong i = 0; i < pci.GetDeviceCount() && InstanceCount < MaxInstances; i++)
    {
        Pci::DeviceInfo* dev = pci.GetDevice(i);
        if (!dev)
            break;

        if (dev->Vendor != Pci::VendorVirtio)
            continue;
        if (dev->Device != Pci::DevVirtioBlk && dev->Device != Pci::DevVirtioBlkModern)
            continue;

        char name[8];
        name[0] = 'v';
        name[1] = 'd';
        name[2] = (char)('a' + InstanceCount);
        name[3] = '\0';

        VirtioBlk& inst = Instances[InstanceCount];
        if (inst.Init(dev, name))
        {
            InstanceCount++;
        }
    }

    Trace(0, "VirtioBlk: initialized %u devices", InstanceCount);
}

/* Global interrupt handler called from assembly stub. */
extern "C" void VirtioBlkInterrupt(Context* ctx)
{
    /* Route to the first instance that has an active interrupt.
       For simplicity, just notify all instances. */
    for (ulong i = 0; i < VirtioBlk::InstanceCount; i++)
    {
        VirtioBlk::Instances[i].Interrupt(ctx);
    }

    Lapic::EOI();
}

}