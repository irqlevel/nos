#pragma once

#include <include/types.h>

namespace Kernel
{

/* Virtio 1.0 legacy virtqueue structures. */

struct VirtqDesc
{
    u64 Addr;   /* Physical address of buffer */
    u32 Len;    /* Length of buffer */
    u16 Flags;  /* VIRTQ_DESC_F_* */
    u16 Next;   /* Next descriptor index if NEXT flag set */

    static const u16 FlagNext = 1;
    static const u16 FlagWrite = 2; /* Buffer is device-writable (read by driver) */
};

static_assert(sizeof(VirtqDesc) == 16, "Invalid size");

struct VirtqAvail
{
    u16 Flags;
    u16 Idx;
    u16 Ring[];
    /* Followed by: u16 UsedEvent (if VIRTIO_F_EVENT_IDX) */
};

struct VirtqUsedElem
{
    u32 Id;
    u32 Len;
};

struct VirtqUsed
{
    u16 Flags;
    u16 Idx;
    VirtqUsedElem Ring[];
    /* Followed by: u16 AvailEvent (if VIRTIO_F_EVENT_IDX) */
};

class VirtQueue
{
public:
    VirtQueue();
    ~VirtQueue();

    bool Setup(u16 queueSize);

    ulong GetPhysAddr();

    /* Add a buffer chain.  Returns the head descriptor index, or -1 on error.
       bufs[] is an array of {physAddr, len, writable} tuples. */
    struct BufDesc
    {
        u64 Addr;   /* Physical address */
        u32 Len;
        bool Writable;
    };

    int AddBufs(BufDesc* bufs, ulong count);

    /* Notify the device that new buffers are available (MMIO). */
    void Kick(volatile void* notifyAddr, u16 queueIdx);

    /* Physical addresses of the three ring components
       (needed by modern virtio-pci queue setup). */
    ulong GetDescPhys();
    ulong GetAvailPhys();
    ulong GetUsedPhys();

    /* Check if there are used buffers to process. */
    bool HasUsed();

    /* Get next used buffer.  Returns the head descriptor id and length.
       Returns false if no used buffers available. */
    bool GetUsed(u32& id, u32& len);

    u16 GetQueueSize();

private:
    VirtQueue(const VirtQueue& other) = delete;
    VirtQueue(VirtQueue&& other) = delete;
    VirtQueue& operator=(const VirtQueue& other) = delete;
    VirtQueue& operator=(VirtQueue&& other) = delete;

    VirtqDesc* Descs;
    VirtqAvail* Avail;
    VirtqUsed* Used;

    u16 QueueSize;
    u16 FreeHead;    /* Head of free descriptor chain */
    u16 NumFree;     /* Number of free descriptors */
    u16 LastUsedIdx; /* Last used index we've seen */

    ulong PhysAddr;  /* Physical address of the queue memory */
    void* VirtAddr;  /* Virtual address of the queue memory */
    ulong TotalPages;
};

}