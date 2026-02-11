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
    : IoBase(0)
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

    /* Read BAR0 -- I/O port base */
    u32 bar0 = pci.GetBAR(pciDev->Bus, pciDev->Slot, pciDev->Func, 0);
    if (!(bar0 & 1))
    {
        Trace(0, "VirtioBlk %s: BAR0 is MMIO, expected I/O port", name);
        return false;
    }
    IoBase = bar0 & 0xFFFC;

    Trace(0, "VirtioBlk %s: BAR0 iobase 0x%p irq %u",
        name, (ulong)IoBase, (ulong)pciDev->InterruptLine);

    /* Enable PCI bus mastering */
    pci.EnableBusMastering(pciDev->Bus, pciDev->Slot, pciDev->Func);

    /* Reset device */
    Outb(IoBase + RegDeviceStatus, 0);

    /* Acknowledge */
    Outb(IoBase + RegDeviceStatus, StatusAcknowledge);

    /* Driver */
    Outb(IoBase + RegDeviceStatus, StatusAcknowledge | StatusDriver);

    /* Read and negotiate features */
    u32 devFeatures = In(IoBase + RegDeviceFeatures);
    Trace(0, "VirtioBlk %s: device features 0x%p", name, (ulong)devFeatures);

    /* We don't need any special features for basic operation. */
    u32 guestFeatures = 0;
    Out(IoBase + RegGuestFeatures, guestFeatures);

    /* Setup virtqueue 0 (request queue) */
    Outw(IoBase + RegQueueSelect, 0);
    u16 queueSize = Inw(IoBase + RegQueueSize);
    Trace(0, "VirtioBlk %s: queue size %u", name, (ulong)queueSize);

    if (queueSize == 0)
    {
        Trace(0, "VirtioBlk %s: queue size is 0", name);
        Outb(IoBase + RegDeviceStatus, StatusFailed);
        return false;
    }

    if (!Queue.Setup(queueSize))
    {
        Trace(0, "VirtioBlk %s: failed to setup queue", name);
        Outb(IoBase + RegDeviceStatus, StatusFailed);
        return false;
    }

    /* Tell device the queue physical page frame number */
    Out(IoBase + RegQueuePfn, (u32)(Queue.GetPhysAddr() / Const::PageSize));

    /* Set DRIVER_OK */
    Outb(IoBase + RegDeviceStatus, StatusAcknowledge | StatusDriver | StatusDriverOk);

    /* Read device config: capacity (u64 at offset 0) */
    u32 capLow = In(IoBase + RegConfig + 0);
    u32 capHigh = In(IoBase + RegConfig + 4);
    CapacitySectors = ((u64)capHigh << 32) | capLow;

    Trace(0, "VirtioBlk %s: capacity %u sectors (%u MB)",
        name, CapacitySectors, (CapacitySectors * 512) / (1024 * 1024));

    /* Allocate pages for DMA buffers (request header, data, status). */
    auto& pt = Mm::PageTable::GetInstance();
    Mm::Page* dmaPage = pt.AllocContiguousPages(2);
    if (!dmaPage)
    {
        Trace(0, "VirtioBlk %s: failed to alloc DMA pages", name);
        Outb(IoBase + RegDeviceStatus, StatusFailed);
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
            Outb(IoBase + RegDeviceStatus, StatusFailed);
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
    Interrupt::Register(*this, acpi.GetGsiByIrq(irq), vector);

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
    Queue.Kick(IoBase + RegQueueNotify);

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

void VirtioBlk::Interrupt(Context* ctx)
{
    (void)ctx;
    InterruptCounter.Inc();

    /* Read ISR status to acknowledge interrupt */
    Inb(IoBase + RegISRStatus);
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

        if (dev->Vendor != Pci::VendorVirtio || dev->Device != Pci::DevVirtioBlk)
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