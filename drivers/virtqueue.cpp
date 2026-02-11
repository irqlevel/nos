#include "virtqueue.h"

#include <kernel/trace.h>
#include <kernel/asm.h>
#include <mm/page_table.h>
#include <mm/memory_map.h>

namespace Kernel
{

VirtQueue::VirtQueue()
    : Descs(nullptr)
    , Avail(nullptr)
    , Used(nullptr)
    , QueueSize(0)
    , FreeHead(0)
    , NumFree(0)
    , LastUsedIdx(0)
    , PhysAddr(0)
    , VirtAddr(nullptr)
    , TotalPages(0)
{
}

VirtQueue::~VirtQueue()
{
}

bool VirtQueue::Setup(u16 queueSize)
{
    QueueSize = queueSize;

    /* Calculate total size per virtio legacy spec:
       Descriptor table: queueSize * 16
       Available ring:   6 + 2 * queueSize
       (padding to next page boundary)
       Used ring:        6 + 8 * queueSize */

    ulong descTableSize = (ulong)queueSize * sizeof(VirtqDesc);
    ulong availSize = sizeof(u16) * 2 + sizeof(u16) * queueSize + sizeof(u16);
    ulong usedSize = sizeof(u16) * 2 + sizeof(VirtqUsedElem) * queueSize + sizeof(u16);

    ulong availEnd = descTableSize + availSize;
    ulong usedOffset = (availEnd + Const::PageSize - 1) & ~(Const::PageSize - 1);
    ulong totalSize = usedOffset + usedSize;

    TotalPages = (totalSize + Const::PageSize - 1) / Const::PageSize;

    Trace(0, "VirtQueue setup size %u pages %u", (ulong)queueSize, TotalPages);

    auto& pt = Mm::PageTable::GetInstance();
    Mm::Page* page = pt.AllocContiguousPages(TotalPages);
    if (!page)
    {
        Trace(0, "VirtQueue: failed to alloc %u contiguous pages", TotalPages);
        return false;
    }

    PhysAddr = page->GetPhyAddress();

    /* Map the contiguous pages into the virtual address space. */
    for (ulong i = 0; i < TotalPages; i++)
    {
        ulong va = page[i].GetPhyAddress() + Mm::MemoryMap::KernelSpaceBase;
        if (!pt.MapPage(va, &page[i]))
        {
            Trace(0, "VirtQueue: failed to map page %u", i);
            return false;
        }
    }

    VirtAddr = (void*)(PhysAddr + Mm::MemoryMap::KernelSpaceBase);

    Trace(0, "VirtQueue phys 0x%p virt 0x%p pages %u", PhysAddr, (ulong)VirtAddr, TotalPages);

    /* Memory is already zeroed by AllocContiguousPages. */

    Descs = (VirtqDesc*)VirtAddr;
    Avail = (VirtqAvail*)((ulong)VirtAddr + descTableSize);
    Used = (VirtqUsed*)((ulong)VirtAddr + usedOffset);

    /* Build free descriptor chain. */
    for (u16 i = 0; i < queueSize - 1; i++)
    {
        Descs[i].Next = i + 1;
    }
    Descs[queueSize - 1].Next = 0xFFFF; /* End of chain */

    FreeHead = 0;
    NumFree = queueSize;
    LastUsedIdx = 0;

    Avail->Flags = 0;
    Avail->Idx = 0;

    return true;
}

ulong VirtQueue::GetPhysAddr()
{
    return PhysAddr;
}

u16 VirtQueue::GetQueueSize()
{
    return QueueSize;
}

int VirtQueue::AddBufs(BufDesc* bufs, ulong count)
{
    if (count == 0 || count > NumFree)
        return -1;

    u16 head = FreeHead;
    u16 idx = head;

    for (ulong i = 0; i < count; i++)
    {
        VirtqDesc* d = &Descs[idx];
        d->Addr = bufs[i].Addr;
        d->Len = bufs[i].Len;
        d->Flags = 0;
        if (bufs[i].Writable)
            d->Flags |= VirtqDesc::FlagWrite;
        if (i < count - 1)
        {
            d->Flags |= VirtqDesc::FlagNext;
            idx = d->Next;
        }
        else
        {
            FreeHead = d->Next;
            d->Next = 0;
        }
    }

    NumFree -= count;

    /* Add to available ring. */
    u16 availIdx = Avail->Idx;
    Avail->Ring[availIdx % QueueSize] = head;
    Barrier();
    Avail->Idx = availIdx + 1;
    Barrier();

    return (int)head;
}

void VirtQueue::Kick(u16 ioPort, u16 queueIdx)
{
    Barrier();
    Outw(ioPort, queueIdx);
}

bool VirtQueue::HasUsed()
{
    Barrier();
    return LastUsedIdx != Used->Idx;
}

bool VirtQueue::GetUsed(u32& id, u32& len)
{
    Barrier();
    if (LastUsedIdx == Used->Idx)
        return false;

    u16 usedIdx = LastUsedIdx % QueueSize;
    id = Used->Ring[usedIdx].Id;
    len = Used->Ring[usedIdx].Len;
    LastUsedIdx++;

    /* Return the descriptors to the free chain. */
    u16 descIdx = (u16)id;
    while (true)
    {
        u16 next = Descs[descIdx].Next;
        bool hasNext = (Descs[descIdx].Flags & VirtqDesc::FlagNext) != 0;
        Descs[descIdx].Flags = 0;
        Descs[descIdx].Next = FreeHead;
        FreeHead = descIdx;
        NumFree++;
        if (!hasNext)
            break;
        descIdx = next;
    }

    return true;
}

}