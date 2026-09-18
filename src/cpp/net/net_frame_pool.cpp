#include "net_frame_pool.h"

extern "C" {

/* What `netpool` prints; crate::frame::Stats is the same struct. */
struct RustNetPoolStats
{
    unsigned int Ready;
    unsigned long Frames;
    unsigned long Capacity;
    unsigned long InRing;
    unsigned long InCaches;
    unsigned long InFlight;
    unsigned long Hits;
    unsigned long Misses;
    unsigned long Oversized;
    unsigned long Refills;
    unsigned long Flushes;
};

int rust_netframe_pool_setup(unsigned long count);
void rust_netframe_pool_stats(RustNetPoolStats* out);
unsigned long rust_netframe_pool_misses();
unsigned long rust_netframe_pool_in_flight();

}

namespace Kernel
{

bool NetFramePool::Setup(ulong frameCount)
{
    return rust_netframe_pool_setup(frameCount) == 0;
}

ulong NetFramePool::GetAllocMisses()
{
    return rust_netframe_pool_misses();
}

ulong NetFramePool::GetInFlight()
{
    return rust_netframe_pool_in_flight();
}

void NetFramePool::Dump(Stdlib::Printer& printer)
{
    RustNetPoolStats stats = {};
    rust_netframe_pool_stats(&stats);

    if (stats.Ready == 0)
    {
        printer.Printf("netpool: not set up\n");
        return;
    }

    printer.Printf("frames %u of %u bytes, %u in the ring, %u in cpu caches\n",
        stats.Frames, stats.Capacity, stats.InRing, stats.InCaches);
    printer.Printf("in flight %u\n", stats.InFlight);
    printer.Printf("alloc hits %u, misses %u, oversized %u\n",
        stats.Hits, stats.Misses, stats.Oversized);
    printer.Printf("ring refills %u, flushes %u\n", stats.Refills, stats.Flushes);
}

}
