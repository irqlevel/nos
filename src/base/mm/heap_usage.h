#pragma once

#include "cpu_cache.h"

#include <include/types.h>

namespace kernel
{

namespace mm
{

/* What one kind of caller holds of the heap, and the most it has held at
   once: the bytes it asked for, what those took -- a pool's block with the
   pool's header in front of it, or a run of pages a power of two long
   (AllocatorImpl::footprint) -- and the blocks; with its allocations and
   frees since boot, and the allocations the heap had nothing for. Rust's
   global allocator keeps one (rust_heap_usage), which `memusage` shows.

   No lock. A change goes into this CPU's slot, a cache line of its own,
   with interrupts off for the few loads and stores it takes: nothing else
   writes the slot meanwhile -- not another CPU, not an interrupt on this
   one -- so not even a locked instruction. The totals every CPU shares are
   reached only when a slot has moved FOLD_BYTES or FOLD_BLOCKS either way
   since it was last folded into them: an atomic add each, and a
   compare-and-swap for a peak that rises. The peaks have to be kept there,
   since the most the sum has ever held is no sum of what each slot held;
   one is the most the totals held at a fold or a read, within a slot's
   worth -- FOLD_BYTES, FOLD_BLOCKS -- a CPU of the true one, either way. */
class HeapUsage final
{
public:
    /* Every member initialised, the destructor trivial: a static one is
       zeroes the compiler lays out, with no constructor to run */
    constexpr HeapUsage() {}

    /* A block handed out: `bytes` asked for, `taken` what it took -- one
       block's, which the heap gave, so far below what a long holds */
    void add(ulong bytes, ulong taken);
    /* That block back, with what add was told of it */
    void sub(ulong bytes, ulong taken);
    /* An allocation the heap had nothing for */
    void refuse();

    struct Totals
    {
        /* Signed: a count below zero is a free nobody allocated, shown as
           what it is */
        long bytes;
        long taken;
        long blocks;
        long bytes_peak;
        long taken_peak;
        long blocks_peak;
        ulong allocs;
        ulong frees;
        ulong refused;
    };

    /* Now, the peaks, and the counts since boot. The sums are of words read
       one at a time while the CPUs go on; a fold on another CPU meanwhile
       may be missing from one, never counted twice. */
    void read(Totals& totals);

    /* How far a slot moves, in bytes asked for or taken, or in blocks,
       before it is folded into the totals */
    static constexpr long FOLD_BYTES = 64 * 1024;
    static constexpr long FOLD_BLOCKS = 256;

private:
    HeapUsage(const HeapUsage& other) = delete;
    HeapUsage(HeapUsage&& other) = delete;
    HeapUsage& operator=(const HeapUsage& other) = delete;
    HeapUsage& operator=(HeapUsage&& other) = delete;

    /* What a CPU changed since its slot was last folded: written by that
       CPU alone, with interrupts off; read by anyone. 128 bytes each, so
       that no CPU's writes take another's line from it: Apple's cores have
       lines that long, and x86's prefetcher fetches lines in pairs (the
       cost measured is in docs/paging.md). */
    struct alignas(128) Slot
    {
        long bytes = 0;
        long taken = 0;
        long allocs = 0;
        long frees = 0;
    };

    void change(long bytes, long taken, long allocs, long frees);
    /* What a slot held, or a change no slot took, into the totals */
    void fold(long bytes, long taken, long allocs, long frees);
    static bool beyond(long value, long limit);
    static void raise(long& peak, long value);

    Slot slots_[CACHE_CPUS];
    long bytes_ = 0;
    long taken_ = 0;
    long blocks_ = 0;
    long allocs_ = 0;
    long frees_ = 0;
    long refused_ = 0;
    long bytes_peak_ = 0;
    long taken_peak_ = 0;
    long blocks_peak_ = 0;
};

/* The Rust heap's: what Rust's global allocator holds (kernel_alloc,
   kernel_free) */
HeapUsage& rust_heap_usage();

}
}
