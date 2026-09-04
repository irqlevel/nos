#pragma once

#include <net/net_frame.h>
#include <kernel/lockless_ring.h>
#include <kernel/cpu.h>
#include <lib/printer.h>

namespace Kernel
{

/* Recycled transmit/receive frames, so that moving a packet costs no
   allocator work at all.
 
   Without this, every frame costs three things that have nothing to do with
   networking: Mm::Alloc, a VirtToPhys walk (which temp-maps a page per level
   and invalidates the TLB for each), and on release an Mm::Free whose page
   allocator shoots down the TLB on every other CPU and waits for all of them
   to answer. The last one is not merely slow -- it is what deadlocked this
   kernel when a driver freed a frame under its TX lock. A datapath that
   never calls the allocator cannot have that bug.
 
   Frames are built once at boot: fixed size, physical address resolved then
   and never again. A per-CPU cache serves the common case with interrupts
   off and no atomics, so a frame released on a CPU is handed back out on the
   same CPU while it is still in that core's cache. Behind the caches is one
   lockless ring, touched only in batches when a cache runs dry or fills. */
class NetFramePool final
{
public:
    static NetFramePool& GetInstance()
    {
        static NetFramePool Instance;
        return Instance;
    }

    /* Data bytes per frame. 2 KiB covers a 1500-byte MTU with room for the
       headers a driver prepends, and keeps the whole frame inside one page. */
    static const ulong FrameCapacity = 2048;

    /* Enough to keep every ring on the box full several times over: the
       drivers use 256-entry rings, and 1024 frames is about 4 MiB of a
       machine that has gigabytes. */
    static const ulong DefaultFrameCount = 1024;

    bool Setup(ulong frameCount);

    /* nullptr when the request is too large for a pooled frame or the pool
       is empty; the caller falls back to the allocator. */
    NetFrame* Alloc(ulong dataLen);

    void Dump(Stdlib::Printer& printer);

private:
    NetFramePool();
    ~NetFramePool();
    NetFramePool(const NetFramePool& other) = delete;
    NetFramePool& operator=(const NetFramePool& other) = delete;

    static void PoolFrameRelease(NetFrame* frame, void* ctx);
    void Release(NetFrame* frame);

    /* How many frames a cache holds, and how many move between a cache and
       the ring at once. Batching is the whole point: the ring is touched
       once per Batch frames instead of once per packet. */
    static const ulong CacheSize = 64;
    static const ulong Batch = 32;

    /* Own cache lines: two CPUs must never share one, and the alignment has
       to be on the type -- padding the size alone leaves the whole array
       free to start mid-line. */
    struct __attribute__((aligned(64))) PerCpuCache
    {
        NetFrame* Frame[CacheSize];
        ulong Count;

        /* Counted here rather than in a shared Atomic: one increment per
           packet on a line every CPU writes would put back exactly the
           contention this pool exists to remove. Summed only by Dump. */
        ulong Hits;
    };

    bool Ready;
    ulong FrameCount;
    LocklessRing Ring;
    LocklessRing::Cell* Cells;
    PerCpuCache Cache[MaxCpus];

    Atomic AllocMisses;
    Atomic Oversized;
    Atomic RingRefills;
    Atomic RingFlushes;

    static const ulong Tag = 'NFPl';
};

}
