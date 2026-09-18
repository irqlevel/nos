#pragma once

#include <include/types.h>
#include <lib/printer.h>

namespace Kernel
{

/* The recycled frame pool is Rust (src/rust/net/src/frame.rs): the per-CPU
   caches, the lockless ring behind them, and the frames built once at boot.
   What is left here is the way in.

   It exists so that moving a packet costs no allocator work at all. Without
   it every frame costs an allocation, a virtual-to-physical walk, and on
   release a free whose page allocator shoots down the TLB on every other CPU
   -- which is what deadlocked this kernel when a driver freed a frame under
   its transmit lock. */
class NetFramePool final
{
public:
    static NetFramePool& GetInstance()
    {
        static NetFramePool Instance;
        return Instance;
    }

    /* Data bytes per frame, and how many are built when the command line
       says nothing (netframes=N overrides it). */
    static const ulong FrameCapacity = 2048;
    static const ulong DefaultFrameCount = 4096;

    bool Setup(ulong frameCount);

    void Dump(Stdlib::Printer& printer);

    /* For a caller that wants to report pool health somewhere Dump cannot
       reach -- a periodic trace, say, on a machine that has stopped
       answering the shell. */
    ulong GetAllocMisses();
    ulong GetInFlight();

private:
    NetFramePool() {}
    ~NetFramePool() {}
    NetFramePool(const NetFramePool& other) = delete;
    NetFramePool& operator=(const NetFramePool& other) = delete;
};

}
