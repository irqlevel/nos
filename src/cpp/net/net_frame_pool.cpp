#include "net_frame_pool.h"

#include <kernel/trace.h>
#include <kernel/panic.h>
#include <kernel/preempt.h>
#include <hal/irqchip.h>
#include <mm/new.h>
#include <mm/page_table.h>

namespace Kernel
{

NetFramePool::NetFramePool()
    : Ready(false)
    , FrameCount(0)
    , Cells(nullptr)
    , AllocMisses(0)
    , Oversized(0)
    , RingRefills(0)
    , RingFlushes(0)
{
    for (ulong i = 0; i < MaxCpus; i++)
    {
        Cache[i].Count = 0;
        Cache[i].Hits = 0;
    }
}

NetFramePool::~NetFramePool()
{
}

bool NetFramePool::Setup(ulong frameCount)
{
    if (Ready || frameCount == 0)
        return false;

    /* The ring has to be able to hold every frame at once -- a flush from a
       full cache must never fail, or a frame would have to be handed back to
       the allocator, which is the thing this exists to avoid. Round up to a
       power of two, which the ring requires anyway. */
    ulong capacity = 1;
    while (capacity < frameCount)
        capacity = capacity * 2;

    Cells = (LocklessRing::Cell*)Mm::Alloc(capacity * sizeof(LocklessRing::Cell), Tag);
    if (Cells == nullptr)
    {
        Trace(0, "NetFramePool: no memory for %u ring cells", capacity);
        return false;
    }

    if (!Ring.Setup(Cells, capacity))
    {
        Mm::Free(Cells);
        Cells = nullptr;
        return false;
    }

    auto& pt = Mm::PageTable::GetInstance();
    ulong built = 0;

    for (ulong i = 0; i < frameCount; i++)
    {
        NetFrame* frame = (NetFrame*)Mm::Alloc(sizeof(NetFrame) + FrameCapacity, Tag);
        if (frame == nullptr)
            break;

        Stdlib::MemSet(frame, 0, sizeof(NetFrame));
        frame->Link.Init();
        frame->Data = (u8*)(frame + 1);
        frame->Length = 0;
        frame->Refcount.Set(0);
        frame->Direction = NetFrame::Tx;
        frame->Release = PoolFrameRelease;
        frame->ReleaseCtx = this;

        /* Once, here, and never again on the datapath. */
        frame->DataPhys = pt.VirtToPhys((ulong)frame->Data);
        if (frame->DataPhys == 0)
        {
            Mm::Free(frame);
            break;
        }

        if (!Ring.Enqueue(frame))
        {
            Mm::Free(frame);
            break;
        }

        built++;
    }

    if (built == 0)
    {
        Mm::Free(Cells);
        Cells = nullptr;
        Trace(0, "NetFramePool: could not build any frames");
        return false;
    }

    FrameCount = built;
    Ready = true;

    Trace(0, "NetFramePool: %u frames of %u bytes, ring capacity %u, %u KiB",
        FrameCount, FrameCapacity, capacity,
        (FrameCount * (sizeof(NetFrame) + FrameCapacity)) / Const::KB);

    return true;
}

NetFrame* NetFramePool::Alloc(ulong dataLen)
{
    if (!Ready)
        return nullptr;

    if (dataLen > FrameCapacity)
    {
        Oversized.Inc();
        return nullptr;
    }

    NetFrame* frame = nullptr;

    {
        /* Interrupts off rather than a lock: the cache belongs to this CPU
           and nothing else touches it, so there is nothing to contend for
           and no atomic to pay for.

           The order matters. Reading the CPU id first and disabling
           interrupts after leaves a window in which this task can be
           preempted onto another CPU, and then two CPUs are inside one
           per-CPU cache at once -- which is not a per-CPU cache at all. */
        ulong flags = PreemptIrqSave();

        ulong index = Hal::GetCurrentCpuHwId();
        if (index >= MaxCpus)
        {
            PreemptIrqRestore(flags);
            return nullptr;
        }

        PerCpuCache& cache = Cache[index];

        if (cache.Count == 0)
        {
            for (ulong i = 0; i < Batch; i++)
            {
                void* got;
                if (!Ring.Dequeue(got))
                    break;

                cache.Frame[cache.Count++] = (NetFrame*)got;
            }

            if (cache.Count != 0)
                RingRefills.Inc();
        }

        if (cache.Count != 0)
        {
            frame = cache.Frame[--cache.Count];
            cache.Hits++;
        }

        PreemptIrqRestore(flags);
    }

    if (frame == nullptr)
    {
        AllocMisses.Inc();
        return nullptr;
    }

    frame->Link.Init();
    frame->Length = 0;
    frame->Refcount.Set(1);
    return frame;
}

void NetFramePool::Release(NetFrame* frame)
{
    /* Put() only calls a release function after the count reached zero, so
       anything else here means the frame was released twice -- and a frame
       in the cache twice is handed to two owners. Catch it where it happens
       rather than where it corrupts. */
    BugOn(frame->Refcount.Get() != 0);

    /* Interrupts off before the CPU id, for the reason spelled out in
       Alloc: read the other way round, the id can be stale by the time it
       indexes the array. */
    ulong flags = PreemptIrqSave();

    ulong index = Hal::GetCurrentCpuHwId();
    if (index >= MaxCpus)
    {
        /* No cache to put it in; the ring always has room for every frame. */
        PreemptIrqRestore(flags);
        Ring.Enqueue(frame);
        return;
    }

    PerCpuCache& cache = Cache[index];

    if (cache.Count == CacheSize)
    {
        for (ulong i = 0; i < Batch; i++)
        {
            if (!Ring.Enqueue(cache.Frame[--cache.Count]))
            {
                /* Sized so this cannot happen; put it back rather than lose
                   the frame if it somehow does. */
                cache.Count++;
                break;
            }
        }

        RingFlushes.Inc();
    }

    if (cache.Count < CacheSize)
        cache.Frame[cache.Count++] = frame;
    else
        Ring.Enqueue(frame);

    PreemptIrqRestore(flags);
}

void NetFramePool::PoolFrameRelease(NetFrame* frame, void* ctx)
{
    ((NetFramePool*)ctx)->Release(frame);
}

void NetFramePool::Dump(Stdlib::Printer& printer)
{
    if (!Ready)
    {
        printer.Printf("netpool: not set up\n");
        return;
    }

    ulong cached = 0;
    ulong hits = 0;
    for (ulong i = 0; i < MaxCpus; i++)
    {
        cached += Cache[i].Count;
        hits += Cache[i].Hits;
    }

    /* Ring.Count() is a snapshot of two independently moving positions, so
       held can read a little high; clamp rather than print a huge number
       that is really a negative one. */
    ulong ring = Ring.Count();
    ulong held = ring + cached;
    ulong inFlight = (FrameCount > held) ? (FrameCount - held) : 0;

    printer.Printf("frames %u of %u bytes, %u in the ring, %u in cpu caches\n",
        FrameCount, FrameCapacity, ring, cached);
    printer.Printf("in flight %u\n", inFlight);
    printer.Printf("alloc hits %u, misses %u, oversized %u\n",
        hits, AllocMisses.Get(), Oversized.Get());
    printer.Printf("ring refills %u, flushes %u\n",
        RingRefills.Get(), RingFlushes.Get());
}

}
