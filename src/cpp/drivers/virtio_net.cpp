#include "virtio_net.h"
#include <hal/irqchip.h>
#include <arch/x86_64/ioapic.h>

#include <kernel/trace.h>
#include <hal/cpu.h>
#include <hal/context.h>
#include <hal/irq_stubs.h>
#include <kernel/panic.h>
#include <kernel/interrupt.h>
#include <arch/x86_64/idt.h>
#include <kernel/softirq.h>
#include <net/arp.h>
#include <net/icmp.h>
#include <net/tcp.h>
#include <mm/new.h>

namespace Kernel
{

using Net::EthHdr;
using Net::IpHdr;
using Net::UdpHdr;
using Net::ArpPacket;
using Net::Htons;
using Net::Htonl;
using Net::Ntohs;
using Net::Ntohl;
using Net::IpChecksum;

VirtioNet VirtioNet::Instances[MaxInstances];
ulong VirtioNet::InstanceCount = 0;

VirtioNet::VirtioNet()
    : Transport(&PciTransport)
    , RxNotifyAddr(nullptr)
    , TxNotifyAddr(nullptr)
    , IntVector(-1)
    , Initialized(false)
    , NetHdrSize(sizeof(VirtioNetHdr)) /* Updated in Init() for legacy */
    , FreeTxSlotMask(0)
    , TxHdrPage(nullptr)
    , TxHdrPagePhys(0)
    , RxSlotCount(0)
    , RxEmptySlots(0)
    , RxHdrPage(nullptr)
    , RxHdrPagePhys(0)
    , RxNeedNotify(false)
{
    DevName[0] = '\0';
    Stdlib::MemSet(TxSlots, 0, sizeof(TxSlots));
    Stdlib::MemSet(TxSlotByHead, 0, sizeof(TxSlotByHead));
    Stdlib::MemSet(RxSlotFrame, 0, sizeof(RxSlotFrame));
    for (ulong i = 0; i < VirtQueue::MaxDescriptors; i++)
        RxSlotByHead[i] = NoRxSlot;
}

VirtioNet::~VirtioNet()
{
}

bool VirtioNet::Init(Pci::DeviceInfo* pciDev, const char* name)
{
    auto& pci = Pci::GetInstance();

    /* Enable PCI bus mastering */
    pci.EnableBusMastering(pciDev->Bus, pciDev->Slot, pciDev->Func);

    /* Probe modern virtio-pci capabilities and map MMIO BARs */
    if (!PciTransport.Probe(pciDev))
    {
        Trace(0, "VirtioNet %s: transport probe failed", name);
        return false;
    }
    Transport = &PciTransport;

    Trace(0, "VirtioNet %s: %s virtio-pci probed, irq %u",
        name, Transport->IsLegacy() ? "legacy" : "modern",
        (ulong)pciDev->InterruptLine);

    return InitCommon(name, pciDev->InterruptLine, (u8)(0x30 + InstanceCount));
}

bool VirtioNet::InitMmio(ulong base, ulong size, u32 intId, const char* name)
{
    if (!MmioTransport.Probe(base, size))
    {
        Trace(0, "VirtioNet %s: mmio probe failed", name);
        return false;
    }
    Transport = &MmioTransport;

    Trace(0, "VirtioNet %s: virtio-mmio probed at 0x%p, intid %u",
        name, base, (ulong)intId);

    return InitCommon(name, (u8)intId, (u8)intId);
}

bool VirtioNet::InitCommon(const char* name, u8 irq, u8 vector)
{
    ulong nameLen = Stdlib::StrLen(name);
    if (nameLen >= sizeof(DevName))
        nameLen = sizeof(DevName) - 1;
    Stdlib::MemCpy(DevName, name, nameLen);
    DevName[nameLen] = '\0';

    /* Reset device */
    Transport->Reset();

    /* Acknowledge */
    Transport->SetStatus(VirtioTransport::StatusAcknowledge);

    /* Driver */
    Transport->SetStatus(VirtioTransport::StatusAcknowledge | VirtioTransport::StatusDriver);

    /* Read and negotiate features (64-bit via select) */
    u32 devFeatures0 = Transport->ReadDeviceFeature(0);
    Trace(0, "VirtioNet %s: device features[0] 0x%p", name, (ulong)devFeatures0);

    u32 drvFeatures0 = 0;
    if (devFeatures0 & FeatureMac)
        drvFeatures0 |= FeatureMac;

    Transport->WriteDriverFeature(0, drvFeatures0);

    if (!Transport->IsLegacy())
    {
        /* features[1]: set VIRTIO_F_VERSION_1 (bit 32 = index 1 bit 0) */
        u32 devFeatures1 = Transport->ReadDeviceFeature(1);
        u32 drvFeatures1 = devFeatures1 & (1 << 0); /* VIRTIO_F_VERSION_1 */
        Transport->WriteDriverFeature(1, drvFeatures1);
    }

    if (!Transport->IsLegacy())
    {
        /* Set FEATURES_OK (modern only; legacy doesn't have this bit) */
        Transport->SetStatus(VirtioTransport::StatusAcknowledge | VirtioTransport::StatusDriver |
                            VirtioTransport::StatusFeaturesOk);

        /* Verify FEATURES_OK is still set */
        if (!(Transport->GetStatus() & VirtioTransport::StatusFeaturesOk))
        {
            Trace(0, "VirtioNet %s: FEATURES_OK not set by device", name);
            Transport->SetStatus(VirtioTransport::StatusFailed);
            return false;
        }
    }

    /* Legacy mode uses 10-byte header (no NumBuffers field) */
    NetHdrSize = Transport->IsLegacy() ? sizeof(VirtioNetHdrLegacy) : sizeof(VirtioNetHdr);
    Trace(0, "VirtioNet %s: net hdr size %u", name, NetHdrSize);

    if (!Transport->IsLegacy() && Transport->IsMsixEnabled())
    {
        u8 vec = Transport->EnableMsixVector(0, *this);
        if (vec == 0)
            Trace(0, "VirtioNet %s: MSI-X unavailable, using INTx", name);
    }

    /* Setup RX virtqueue (queue 0) */
    Transport->SelectQueue(0);
    u16 rxQueueSize = Transport->GetQueueSize();
    Trace(0, "VirtioNet %s: RX queue size %u", name, (ulong)rxQueueSize);

    if (rxQueueSize == 0)
    {
        Trace(0, "VirtioNet %s: RX queue size is 0", name);
        Transport->SetStatus(VirtioTransport::StatusFailed);
        return false;
    }

    if (!HwRxQueue.Setup(rxQueueSize))
    {
        Trace(0, "VirtioNet %s: failed to setup RX queue", name);
        Transport->SetStatus(VirtioTransport::StatusFailed);
        return false;
    }

    Transport->SetQueueDesc(HwRxQueue.GetDescPhys());
    Transport->SetQueueDriver(HwRxQueue.GetAvailPhys());
    Transport->SetQueueDevice(HwRxQueue.GetUsedPhys());
    Transport->EnableQueue();

    if (!Transport->IsLegacy())
        RxNotifyAddr = Transport->GetNotifyAddr(0);

    /* Setup TX virtqueue (queue 1) */
    Transport->SelectQueue(1);
    u16 txQueueSize = Transport->GetQueueSize();
    Trace(0, "VirtioNet %s: TX queue size %u", name, (ulong)txQueueSize);

    if (txQueueSize == 0)
    {
        Trace(0, "VirtioNet %s: TX queue size is 0", name);
        Transport->SetStatus(VirtioTransport::StatusFailed);
        return false;
    }

    if (!HwTxQueue.Setup(txQueueSize))
    {
        Trace(0, "VirtioNet %s: failed to setup TX queue", name);
        Transport->SetStatus(VirtioTransport::StatusFailed);
        return false;
    }

    Transport->SetQueueDesc(HwTxQueue.GetDescPhys());
    Transport->SetQueueDriver(HwTxQueue.GetAvailPhys());
    Transport->SetQueueDevice(HwTxQueue.GetUsedPhys());
    Transport->EnableQueue();

    if (!Transport->IsLegacy())
        TxNotifyAddr = Transport->GetNotifyAddr(1);

    /* Set DRIVER_OK */
    u8 okStatus = VirtioTransport::StatusAcknowledge | VirtioTransport::StatusDriver |
                  VirtioTransport::StatusDriverOk;
    if (!Transport->IsLegacy())
        okStatus |= VirtioTransport::StatusFeaturesOk;
    Transport->SetStatus(okStatus);

    /* Read MAC address from device config */
    if (drvFeatures0 & FeatureMac)
    {
        u8 macBytes[6];
        for (ulong i = 0; i < 6; i++)
            macBytes[i] = Transport->ReadDevCfg8(i);
        Mac = Net::MacAddress(macBytes);
    }

    Trace(0, "VirtioNet %s: MAC %p:%p:%p:%p:%p:%p",
        name,
        (ulong)Mac.Bytes[0], (ulong)Mac.Bytes[1], (ulong)Mac.Bytes[2],
        (ulong)Mac.Bytes[3], (ulong)Mac.Bytes[4], (ulong)Mac.Bytes[5]);

    /* Allocate DMA page for TX slot headers (8 slots, each NetHdrSize bytes) */
    TxHdrPage = (u8*)Mm::AllocMapPages(1, &TxHdrPagePhys);
    if (!TxHdrPage)
    {
        Trace(0, "VirtioNet %s: failed to alloc TX header page", name);
        Transport->SetStatus(VirtioTransport::StatusFailed);
        return false;
    }
    Stdlib::MemSet(TxHdrPage, 0, Const::PageSize);

    /* Init TX slot pool */
    for (ulong s = 0; s < MaxTxSlots; s++)
    {
        TxSlots[s].HdrBuf = TxHdrPage + s * NetHdrSize;
        TxSlots[s].HdrBufPhys = TxHdrPagePhys + s * NetHdrSize;
        TxSlots[s].Frame = nullptr;
        TxSlots[s].Head = -1;
    }
    FreeTxSlotMask = (1UL << MaxTxSlots) - 1; /* all slots free */

    /* One DMA page for every RX slot's virtio-net header */
    static_assert(MaxRxSlots * sizeof(VirtioNetHdr) <= Const::PageSize, "RX headers fit their page");
    static_assert(MaxTxSlots * sizeof(VirtioNetHdr) <= Const::PageSize, "TX headers fit their page");
    static_assert(MaxTxSlots < sizeof(FreeTxSlotMask) * 8, "TX slots are a bit mask");

    RxHdrPage = (u8*)Mm::AllocMapPages(1, &RxHdrPagePhys);
    if (!RxHdrPage)
    {
        Trace(0, "VirtioNet %s: failed to alloc RX header page", name);
        Mm::UnmapFreePages(TxHdrPage);
        TxHdrPage = nullptr;
        Transport->SetStatus(VirtioTransport::StatusFailed);
        return false;
    }
    Stdlib::MemSet(RxHdrPage, 0, Const::PageSize);

    /* Two descriptors to a slot */
    RxSlotCount = rxQueueSize / 2;
    if (RxSlotCount > MaxRxSlots)
        RxSlotCount = MaxRxSlots;

    /* Default IP for QEMU user-mode networking */
    Ip = Net::IpAddress(10, 0, 2, 15);

    Initialized = true;

    /* Pre-post RX frames */
    for (ulong s = 0; s < RxSlotCount; s++)
    {
        if (!PostRxSlot(s))
            RxEmptySlots++;
    }
    Transport->NotifyQueue(0);

    Trace(0, "VirtioNet %s: %u of %u RX slots posted, %u TX slots", name,
        RxSlotCount - RxEmptySlots, RxSlotCount, (ulong)MaxTxSlots);

    if (!Transport->UsingMsix())
    {
        Interrupt::RegisterLevel(*this, irq, vector);
    }

    /* Register as net device */
    NetDeviceTable::GetInstance().Register(this);

    Trace(0, "VirtioNet %s: initialized", name);
    return true;
}

bool VirtioNet::PostRxFrame(ulong slot, NetFrame* frame)
{
    /* The header in a descriptor of its own, which is also the layout a
       legacy device without ANY_LAYOUT asks for; the packet then starts at
       the frame's first byte, where the rest of the stack expects it. */
    VirtQueue::BufDesc bufs[2];
    bufs[0].Addr = RxHdrPagePhys + slot * NetHdrSize;
    bufs[0].Len = (u32)NetHdrSize;
    bufs[0].Writable = true;
    bufs[1].Addr = frame->DataPhys;
    bufs[1].Len = (u32)RxFrameSize;
    bufs[1].Writable = true;

    int head = HwRxQueue.AddBufs(bufs, 2);
    if (head < 0)
        return false;

    /* A head is an index into a queue VirtQueue::Setup held to MaxDescriptors */
    RxSlotByHead[head] = slot;
    RxSlotFrame[slot] = frame;
    return true;
}

bool VirtioNet::PostRxSlot(ulong slot)
{
    NetFrame* frame = NetFrame::AllocTx(RxFrameSize);
    if (frame == nullptr)
        return false;

    frame->Direction = NetFrame::Rx;
    if (!PostRxFrame(slot, frame))
    {
        frame->Put();
        return false;
    }
    return true;
}

void VirtioNet::RefillRx()
{
    for (ulong s = 0; s < RxSlotCount && RxEmptySlots != 0; s++)
    {
        if (RxSlotFrame[s] != nullptr)
            continue;

        /* Still nothing to post it with: the next pass tries again. */
        if (!PostRxSlot(s))
            break;

        RxEmptySlots--;
        RxNeedNotify = true;
    }
}

/* --- TX slot management (caller holds TxQueueLock) --- */

int VirtioNet::AllocTxSlot()
{
    if (FreeTxSlotMask == 0)
        return -1;

    ulong i = (ulong)__builtin_ctzl(FreeTxSlotMask);
    FreeTxSlotMask &= ~(1UL << i);
    return (int)i;
}

void VirtioNet::FreeTxSlot(int idx)
{
    TxSlots[idx].Frame = nullptr;
    TxSlots[idx].Head = -1;
    FreeTxSlotMask |= (1UL << (ulong)idx);
}

/* --- TX: drain SW TxQueue to hardware (caller holds TxQueueLock) --- */

void VirtioNet::FlushTx()
{
    bool submitted = false;

    while (!TxQueue.IsEmpty())
    {
        int slotIdx = AllocTxSlot();
        if (slotIdx < 0)
            break; /* all DMA slots in-flight */

        Stdlib::ListEntry* entry = TxQueue.RemoveHead();
        TxCount--;
        NetFrame* frame = CONTAINING_RECORD(entry, NetFrame, Link);

        /* Fill slot header with zeroed virtio_net_hdr */
        Stdlib::MemSet(TxSlots[slotIdx].HdrBuf, 0, NetHdrSize);

        /* Build 2-descriptor chain: [hdr, data] */
        VirtQueue::BufDesc descs[2];
        descs[0].Addr = TxSlots[slotIdx].HdrBufPhys;
        descs[0].Len = (u32)NetHdrSize;
        descs[0].Writable = false;
        descs[1].Addr = frame->DataPhys;
        descs[1].Len = (u32)frame->Length;
        descs[1].Writable = false;

        int head = HwTxQueue.AddBufs(descs, 2);
        if (head < 0 || (ulong)head >= VirtQueue::MaxDescriptors)
        {
            FreeTxSlot(slotIdx);
            TxQueue.InsertHead(&frame->Link);
            TxCount++;
            break;
        }

        TxSlots[slotIdx].Frame = frame;
        TxSlots[slotIdx].Head = head;
        TxSlotByHead[head] = &TxSlots[slotIdx];
        submitted = true;
    }

    if (submitted)
        Transport->NotifyQueue(1);

    if (!TxQueue.IsEmpty())
        SoftIrq::GetInstance().Raise(SoftIrq::TypeNetTx);
}

/* --- TX: complete hardware TX (caller holds TxQueueLock) --- */

void VirtioNet::CompleteTx()
{
    u32 usedId, usedLen;
    while (HwTxQueue.GetUsed(usedId, usedLen))
    {
        if (usedId >= VirtQueue::MaxDescriptors)
            continue;

        TxSlot* slot = TxSlotByHead[usedId];
        if (!slot)
            continue;

        /* Clear the mapping so a duplicated/spurious used-ring entry for
           this head cannot double-free the slot and its frame. */
        TxSlotByHead[usedId] = nullptr;

        NetFrame* frame = slot->Frame;
        FreeTxSlot((int)(slot - TxSlots));
        if (frame)
            TxDone(frame);
    }
}

/* --- TX: called from softirq to retry pending TX --- */

void VirtioNet::DrainTx()
{
    ulong flags = TxQueueLock.LockIrqSave();
    CompleteTx();
    FlushTx();
    TxQueueLock.UnlockIrqRestore(flags);

    /* Only now: the frames CompleteTx finished with are released here, off
       the lock. The interrupt handler already defers TX completion to this
       soft IRQ for that reason; the deferral was necessary but not
       sufficient, because the soft IRQ then took the lock and freed under
       it anyway. */
    ReleaseTxDone();
}

/* --- RX: reap completed buffers from HW into SW RxQueue --- */

void VirtioNet::ReapRx()
{
    while (HwRxQueue.HasUsed())
    {
        u32 usedId, usedLen;
        if (!HwRxQueue.GetUsed(usedId, usedLen))
            break;

        RxPktCount.Inc();

        /* Map the completed chain back to its slot (heads and slots part
           company once descriptors are recycled). */
        ulong slot = (usedId < VirtQueue::MaxDescriptors) ? RxSlotByHead[usedId] : NoRxSlot;
        if (slot >= RxSlotCount || RxSlotFrame[slot] == nullptr)
        {
            RxDropCount.Inc();
            continue;
        }

        RxSlotByHead[usedId] = NoRxSlot;
        NetFrame* frame = RxSlotFrame[slot];
        RxSlotFrame[slot] = nullptr;
        RxNeedNotify = true;

        if (usedLen <= NetHdrSize)
        {
            RxDropCount.Inc();
            if (!PostRxFrame(slot, frame))
            {
                frame->Put();
                RxEmptySlots++;
            }
            continue;
        }

        /* usedLen is device-controlled; clamp to the frame so a bogus length
           cannot make frame->Length exceed it and let protocol parsers read
           past its end. */
        ulong len = usedLen - NetHdrSize;
        if (len > RxFrameSize)
            len = RxFrameSize;

        frame->Length = len;
        frame->Direction = NetFrame::Rx;

        /* The slot's next frame first. With none to be had -- or no room in
           the queue -- the packet is dropped and its frame goes straight
           back: a slot left empty is one the device can never fill, and a
           ring gone empty raises no interrupt to come back for it, deaf
           until something happens to transmit. */
        NetFrame* fresh = NetFrame::AllocTx(RxFrameSize);
        if (fresh == nullptr || !EnqueueRx(frame))
        {
            RxDropCount.Inc();
            if (fresh != nullptr)
                fresh->Put();
            if (!PostRxFrame(slot, frame))
            {
                frame->Put();
                RxEmptySlots++;
            }
            continue;
        }

        /* Handed up, the stack's now; the fresh frame takes its place. */
        fresh->Direction = NetFrame::Rx;
        if (!PostRxFrame(slot, fresh))
        {
            fresh->Put();
            RxEmptySlots++;
        }
    }

    if (RxEmptySlots != 0)
        RefillRx();
}

/* --- RX: process frames from SW RxQueue (protocol dispatch) --- */

void VirtioNet::ProcessRx()
{
    /* The shared drain, not a copy of it. This driver carried its own for
       long enough to miss two fixes made to the original: frames are taken
       off the queue in one splice rather than one lock acquisition each, and
       a UDP listener's callback runs with the listener table unlocked
       instead of with interrupts off for its duration.

       It also means QEMU exercises the path the bare metal drivers use.
       While the copy existed, nothing here ran it -- the Rust NIC bridge is
       the only other user, and that hardware is not something a test can
       boot. */
    DrainRxQueueAndDispatch();

    if (RxNeedNotify)
    {
        Transport->NotifyQueue(0);
        RxNeedNotify = false;
    }
}

/* --- Interface methods --- */

const char* VirtioNet::GetName()
{
    return DevName;
}

u64 VirtioNet::GetTxPackets()
{
    NetStats st;
    GetTxProtoTotals(st);
    return st.TxTotal;
}

u64 VirtioNet::GetRxPackets()
{
    return (u64)RxPktCount.Get();
}

u64 VirtioNet::GetRxDropped()
{
    return (u64)RxDropCount.Get();
}

void VirtioNet::GetStats(NetStats& stats)
{
    GetTxProtoTotals(stats);
    stats.RxTotal = (u64)RxPktCount.Get();
    GetRxProtoTotals(stats);
    stats.RxDrop += (u64)RxDropCount.Get();
}

/* --- Interrupt --- */

void VirtioNet::OnInterruptRegister(u8 irq, u8 vector)
{
    (void)irq;
    IntVector = vector;
    Trace(0, "VirtioNet %s: interrupt registered vector 0x%p", DevName, (ulong)vector);
}

InterruptHandlerFn VirtioNet::GetHandlerFn()
{
    return VirtioNetInterruptStub;
}

void VirtioNet::OnInterrupt(Context* ctx)
{
    /* Called by shared interrupt dispatch (no EOI here). */
    Interrupt(ctx);
}

void VirtioNet::Interrupt(Context* ctx)
{
    (void)ctx;

    /* Acknowledge the interrupt on the INTx path only. Under MSI-X the ISR
       status register is not the notification mechanism (virtio 1.x 4.1.4.5)
       and a spec-conforming device leaves it 0, so gating on it there would
       drop every RX/TX notification. */
    if (!Transport->UsingMsix())
    {
        u8 isr = Transport->ReadISR();
        if (isr == 0)
            return;
    }

    InterruptStats::Inc(IrqVirtioNet);

    /* Defer TX completion + flush and RX processing to soft IRQ.
     * CompleteTx() calls frame->Put() which may call Mm::Free(),
     * and Mm::Free() can trigger a TLB shootdown IPI.  If another
     * CPU has IRQs disabled (e.g. holding a spinlock), the IPI
     * cannot be acknowledged, causing a deadlock.  By deferring to
     * the soft IRQ task (which runs with IRQs enabled), the free
     * is safe. */
    SoftIrq::GetInstance().Raise(SoftIrq::TypeNetTx);
    SoftIrq::GetInstance().Raise(SoftIrq::TypeNetRx);
}

/* --- InitAll --- */

void VirtioNet::InitAll()
{
    auto& pci = Pci::GetInstance();
    InstanceCount = 0;

    for (ulong i = 0; i < MaxInstances; i++)
        new (&Instances[i]) VirtioNet();

    for (ulong i = 0; i < pci.GetDeviceCount() && InstanceCount < MaxInstances; i++)
    {
        Pci::DeviceInfo* dev = pci.GetDevice(i);
        if (!dev)
            break;

        if (dev->Vendor != Pci::VendorVirtio)
            continue;
        if (dev->Device != Pci::DevVirtioNetwork && dev->Device != Pci::DevVirtioNetModern)
            continue;

        char name[8];
        name[0] = 'e';
        name[1] = 't';
        name[2] = 'h';
        name[3] = (char)('0' + InstanceCount);
        name[4] = '\0';

        VirtioNet& inst = Instances[InstanceCount];
        if (inst.Init(dev, name))
        {
            InstanceCount++;
        }
    }

    /* The TypeNetRx/TypeNetTx softirq handlers are registered by
       NetDeviceTable and dispatch to every registered device. */
    Trace(0, "VirtioNet: initialized %u devices", InstanceCount);
}

void VirtioNet::InitAllMmio(const VirtioMmioSlot* slots, ulong count)
{
    InstanceCount = 0;

    for (ulong i = 0; i < MaxInstances; i++)
        new (&Instances[i]) VirtioNet();

    for (ulong i = 0; i < count && InstanceCount < MaxInstances; i++)
    {
        if (slots[i].DeviceId != 1 /* virtio-net */)
            continue;

        char name[8];
        name[0] = 'e';
        name[1] = 't';
        name[2] = 'h';
        name[3] = (char)('0' + InstanceCount);
        name[4] = '\0';

        VirtioNet& inst = Instances[InstanceCount];
        if (inst.InitMmio(slots[i].Base, slots[i].Size, slots[i].IntId, name))
            InstanceCount++;
    }

    Trace(0, "VirtioNet: initialized %u devices", InstanceCount);
}

/* Global interrupt handler called from assembly stub. */
extern "C" void VirtioNetInterrupt(Context* ctx)
{
    for (ulong i = 0; i < VirtioNet::InstanceCount; i++)
    {
        VirtioNet::Instances[i].Interrupt(ctx);
    }

    Hal::IrqEoi();
}

}
