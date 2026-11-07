#include "heap_usage.h"

#include <hal/cpu.h>

namespace kernel
{

namespace mm
{

void HeapUsage::add(ulong bytes, ulong taken)
{
    change(static_cast<long>(bytes), static_cast<long>(taken), 1, 0);
}

void HeapUsage::sub(ulong bytes, ulong taken)
{
    change(-static_cast<long>(bytes), -static_cast<long>(taken), 0, 1);
}

void HeapUsage::refuse()
{
    __atomic_add_fetch(&refused_, 1, __ATOMIC_RELAXED);
}

bool HeapUsage::beyond(long value, long limit)
{
    return value >= limit || value <= -limit;
}

void HeapUsage::raise(long& peak, long value)
{
    long seen = __atomic_load_n(&peak, __ATOMIC_RELAXED);
    /* A failed exchange leaves in seen what another CPU raised it to */
    while (value > seen &&
           !__atomic_compare_exchange_n(&peak, &seen, value, true, __ATOMIC_RELAXED, __ATOMIC_RELAXED))
    {
    }
}

void HeapUsage::fold(long bytes, long taken, long allocs, long frees)
{
    /* Release: a read that finds a total with this in it finds the slot's
       zeroes, stored before, too -- and does not count it twice (read) */
    raise(bytes_peak_, __atomic_add_fetch(&bytes_, bytes, __ATOMIC_RELEASE));
    raise(taken_peak_, __atomic_add_fetch(&taken_, taken, __ATOMIC_RELEASE));
    raise(blocks_peak_, __atomic_add_fetch(&blocks_, allocs - frees, __ATOMIC_RELEASE));
    __atomic_add_fetch(&allocs_, allocs, __ATOMIC_RELEASE);
    __atomic_add_fetch(&frees_, frees, __ATOMIC_RELEASE);
}

void HeapUsage::change(long bytes, long taken, long allocs, long frees)
{
    /* Interrupts off: the CPU is this one until they are back on, and
       nothing else writes its slot meanwhile -- no other CPU, and no
       interrupt on this one -- so a load and a store a word will do */
    const ulong flags = hal::irq_save();
    ulong cpu = 0;
    if (cache_cpu(cpu))
    {
        Slot& slot = slots_[cpu];
        const long b = __atomic_load_n(&slot.bytes, __ATOMIC_RELAXED) + bytes;
        const long t = __atomic_load_n(&slot.taken, __ATOMIC_RELAXED) + taken;
        const long a = __atomic_load_n(&slot.allocs, __ATOMIC_RELAXED) + allocs;
        const long f = __atomic_load_n(&slot.frees, __ATOMIC_RELAXED) + frees;
        const bool due = beyond(b, FOLD_BYTES) || beyond(t, FOLD_BYTES) || beyond(a - f, FOLD_BLOCKS);
        /* Due: the slot's zeroes first, then the totals (fold) */
        __atomic_store_n(&slot.bytes, due ? 0 : b, __ATOMIC_RELAXED);
        __atomic_store_n(&slot.taken, due ? 0 : t, __ATOMIC_RELAXED);
        __atomic_store_n(&slot.allocs, due ? 0 : a, __ATOMIC_RELAXED);
        __atomic_store_n(&slot.frees, due ? 0 : f, __ATOMIC_RELAXED);
        if (due)
            fold(b, t, a, f);
    }
    else
    {
        /* No slot to say whose: the heap's first allocations, before a CPU
           can tell which it is (cache_cpu), or a CPU past the slots. Into
           the totals at once, which any CPU may add to. */
        fold(bytes, taken, allocs, frees);
    }
    hal::irq_restore(flags);
}

void HeapUsage::read(Totals& totals)
{
    /* The totals first, then the slots: a fold whose add a total read here
       has in it zeroed its slot before (change, fold) */
    long bytes = __atomic_load_n(&bytes_, __ATOMIC_ACQUIRE);
    long taken = __atomic_load_n(&taken_, __ATOMIC_ACQUIRE);
    long blocks = __atomic_load_n(&blocks_, __ATOMIC_ACQUIRE);
    long allocs = __atomic_load_n(&allocs_, __ATOMIC_ACQUIRE);
    long frees = __atomic_load_n(&frees_, __ATOMIC_ACQUIRE);
    for (auto& slot : slots_)
    {
        const long a = __atomic_load_n(&slot.allocs, __ATOMIC_RELAXED);
        const long f = __atomic_load_n(&slot.frees, __ATOMIC_RELAXED);
        bytes += __atomic_load_n(&slot.bytes, __ATOMIC_RELAXED);
        taken += __atomic_load_n(&slot.taken, __ATOMIC_RELAXED);
        blocks += a - f;
        allocs += a;
        frees += f;
    }

    /* What the read summed is a moment of the totals, near enough -- within
       what moved between CPUs while it read -- so a peak below it is raised
       to it: no read shows a peak under the now an earlier read showed */
    raise(bytes_peak_, bytes);
    raise(taken_peak_, taken);
    raise(blocks_peak_, blocks);

    totals.bytes = bytes;
    totals.taken = taken;
    totals.blocks = blocks;
    totals.bytes_peak = __atomic_load_n(&bytes_peak_, __ATOMIC_RELAXED);
    totals.taken_peak = __atomic_load_n(&taken_peak_, __ATOMIC_RELAXED);
    totals.blocks_peak = __atomic_load_n(&blocks_peak_, __ATOMIC_RELAXED);
    totals.allocs = static_cast<ulong>(allocs);
    totals.frees = static_cast<ulong>(frees);
    totals.refused = static_cast<ulong>(__atomic_load_n(&refused_, __ATOMIC_RELAXED));
}

HeapUsage& rust_heap_usage()
{
    /* Constant-initialised: laid out zero, no constructor and no guard */
    static HeapUsage usage;
    return usage;
}

}
}
