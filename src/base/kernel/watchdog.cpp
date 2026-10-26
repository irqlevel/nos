#include "watchdog.h"
#include <hal/cpu.h>
#include "preempt.h"
#include "time.h"
#include "panic.h"
#include "trace.h"
#include "cpu.h"

static_assert(kernel::Watchdog::SLICE_CPUS == kernel::MAX_CPUS,
    "SLICE_CPUS must track MAX_CPUS");

namespace kernel
{

ulong Watchdog::ListLock::lock_irq_save()
{
    ulong flags = preempt_irq_save();
    for (;;)
    {
        if (value_.cmpxchg(1, 0) == 0)
            break;

        cpu_relax();
    }
    return flags;
}

void Watchdog::ListLock::unlock_irq_restore(ulong flags)
{
    value_.set(0);
    preempt_irq_restore(flags);
}

namespace
{

/* Kernighan: one iteration per set bit, so at most the CPU count. Called once
   per walk, not per lock. */
ulong pop_count(ulong value)
{
    ulong count = 0;
    while (value != 0)
    {
        value &= value - 1;
        count++;
    }
    return count;
}

}

Watchdog::Watchdog()
{
    for (ulong i = 0; i < SLICE_CPUS; i++)
    {
        cpu_slice_[i].first = 0;
        cpu_slice_[i].stride = 0;
    }
}

Watchdog::~Watchdog()
{
}

void Watchdog::check()
{
    /* Same counter SpinLock::lock stamps with. A lock taken on another CPU
       is timed against this CPU's counter: on a machine whose cycle
       counters do not run together that can over- or under-state the hold
       time, which only ever costs this trace a false or missed report. */
    u64 now = hal::read_cycle_counter();
    const ulong timeout_ns = 25 * consts::NANO_SECS_IN_MS;

    /* Past this the lock is not slow, it is stuck: nothing in this kernel
       legitimately holds a spinlock for ten seconds. Reporting it as one more
       trace line is not enough -- the line goes into rings whose way out may
       be the very lock that is stuck, which is exactly how the machine dies
       in silence. Panic instead: the panic path is allowed to send without
       that lock, so the report actually leaves. */
    const ulong stuck_ns = 10 * consts::NANO_SECS_IN_SEC;

    /* Divide the table among the CPUs instead of having each of them walk
       all of it. The list is global and a bucket needs looking at once, so
       twenty CPUs each walking all 512 buckets a hundred times a second was
       doing the same work twenty times over -- and contending on the same
       bucket locks to do it. Every bucket is still visited every tick, by
       exactly one CPU: the coverage and the detection latency do not change,
       only the redundancy. */
    ulong first = 0;
    ulong stride = 1;

    auto& table = CpuTable::get_instance();
    ulong mask = table.get_running_cpus_no_lock();
    ulong index = table.get_current_cpu().get_index();

    if (index < MAX_CPUS && (mask & (1UL << index)) != 0)
    {
        /* Rank among the running CPUs rather than the index itself: on x86
           the index is the APIC ID, which a hybrid part leaves full of holes
           (0,1,8,9,...,62 for twenty threads). Ranks are dense, so every
           bucket falls to exactly one CPU. */
        stride = pop_count(mask);
        first = pop_count(mask & ((1UL << index) - 1));
    }

    /* Recorded so `watchdog` can show how the table is divided. Written only
       when the split changes, which after bring-up is never: a store every
       tick would put every CPU back on a shared line. */
    if (index < SLICE_CPUS &&
        (cpu_slice_[index].first != first || cpu_slice_[index].stride != stride))
    {
        cpu_slice_[index].first = first;
        cpu_slice_[index].stride = stride;
    }

    /* Once per walk. This used to be incremented for every lock examined --
       one global atomic, touched by every CPU, as many times a second as
       there are watched locks times the tick rate. */
    check_counter_.inc();

    RawSpinLock* stuck = nullptr;
    ulong stuck_held_ns = 0;

    for (size_t i = first; i < stdlib::array_size(spin_lock_list_); i += stride)
    {
        auto& list_lock = spin_lock_list_lock_[i];
        auto& list = spin_lock_list_[i];

        if (list.is_empty())
            continue;

        ulong flags = list_lock.lock_irq_save();
        for (stdlib::ListEntry* entry = list.flink;
            entry != &list;
            entry = entry->flink)
        {
            RawSpinLock* lock = CONTAINING_RECORD(entry, RawSpinLock, watchdog_list_entry_);
            u64 lock_time = lock->watchdog_lock_time_.get();
            if (lock_time != 0 && now > lock_time)
            {
                /* 0 until the counter's rate is known -- no report then,
                   rather than a report built on a guessed frequency. */
                ulong delta_ns = cycle_counter_delta_to_ns(now - lock_time);
                if (delta_ns > timeout_ns && lock->watchdog_reported_.cmpxchg(1, 0) == 0)
                {
                    trace(0, "Spinlock 0x%p is held too long %lu", lock, delta_ns);
                }

                if (delta_ns > stuck_ns && stuck == nullptr)
                {
                    stuck = lock;
                    stuck_held_ns = delta_ns;
                }
            }
        }
        list_lock.unlock_irq_restore(flags);

        /* Outside the list lock: Panic prints, and printing must not run with
           this held. */
        if (stuck != nullptr)
            PANIC("Spinlock 0x%lX stuck, held %lu ns", (ulong)stuck, stuck_held_ns);
    }
}

void Watchdog::register_spin_lock(RawSpinLock& lock)
{
    size_t i = stdlib::hash_ptr(&lock) % stdlib::array_size(spin_lock_list_);
    auto& list_lock = spin_lock_list_lock_[i];
    auto& list = spin_lock_list_[i];

    ulong flags = list_lock.lock_irq_save();
    list.insert_tail(&lock.watchdog_list_entry_);
    spin_lock_counter_.inc();
    list_lock.unlock_irq_restore(flags);
}

void Watchdog::unregister_spin_lock(RawSpinLock& lock)
{
    size_t i = stdlib::hash_ptr(&lock) % stdlib::array_size(spin_lock_list_);
    auto& list_lock = spin_lock_list_lock_[i];

    ulong flags = list_lock.lock_irq_save();
    if (!lock.watchdog_list_entry_.is_empty())
    {
        lock.watchdog_list_entry_.remove();
        spin_lock_counter_.dec();
    }
    list_lock.unlock_irq_restore(flags);
}

void Watchdog::dump(stdlib::Printer& printer)
{
    printer.printf("locks watched: %lu\n", spin_lock_counter_.get());
    printer.printf("table walks:   %lu\n", check_counter_.get());
    printer.printf("buckets:       %lu\n", (ulong)stdlib::array_size(spin_lock_list_));

    /* Which slice of the table each CPU walks. The slices have to be dense
       and disjoint, or a bucket belongs to nobody and a lock stuck in it is
       never noticed. */
    for (ulong i = 0; i < SLICE_CPUS; i++)
    {
        if (cpu_slice_[i].stride == 0)
            continue;

        printer.printf("cpu %lu: %lu buckets, from %lu every %lu\n", i,
            (ulong)((stdlib::array_size(spin_lock_list_) - cpu_slice_[i].first +
                cpu_slice_[i].stride - 1) / cpu_slice_[i].stride),
            cpu_slice_[i].first, cpu_slice_[i].stride);
    }
}

}