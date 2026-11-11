#include "watchdog.h"
#include <hal/cpu.h>
#include "preempt.h"
#include "time.h"
#include "panic.h"
#include "klog.h"
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
       one CPU, while they all tick.

       Which CPU moves on by one every tick. A slice that was a CPU's for
       good was nobody's the moment that CPU stopped ticking -- wedged with
       its interrupts off, which is what a lock held for ever by it, or
       waited for by it, is -- and the stuck lock, when its bucket was in
       that slice, was never looked at again: no line, no panic at ten
       seconds, one time in as many as there are CPUs and every time on a
       machine of two with both in the deadlock's embrace but one. Rotated,
       a slice whose CPU has stopped is the next CPU's a tick later. */
    ulong first = 0;
    ulong stride = 1;
    ulong rank = 0;

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
        rank = pop_count(mask & ((1UL << index) - 1));
        /* The same tick's number on every CPU -- the clock's, not a count of
           this CPU's own ticks -- so that at any one tick the CPUs' slices
           are still all different */
        first = (rank + get_boot_time().get_value() / ROTATE_NS) % stride;
    }

    /* Recorded so `watchdog` can show how the table is divided. Written only
       when the split changes, which after bring-up is never: a store every
       tick would put every CPU back on a shared line. */
    if (index < SLICE_CPUS &&
        (cpu_slice_[index].first != rank || cpu_slice_[index].stride != stride))
    {
        cpu_slice_[index].first = rank;
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

        /* The locks found held too long, said once the bucket's lock is
           down: a line is a console's write -- ten milliseconds of a
           framebuffer's repaint, with interrupts off and every lock made or
           unmade in this bucket waiting -- and the lock it is about may be
           one the log itself takes, which would keep this CPU here, and the
           panic below from ever being reached. One past the room is said at
           the next walk: it is not marked reported. */
        const RawSpinLock* late[LATE_MAX];
        ulong late_ns[LATE_MAX];
        ulong late_count = 0;

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
                if (delta_ns > timeout_ns && late_count < LATE_MAX &&
                    lock->watchdog_reported_.cmpxchg(1, 0) == 0)
                {
                    late[late_count] = lock;
                    late_ns[late_count] = delta_ns;
                    late_count++;
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
           this held. The panic first: it says more, and does not need the
           log's locks to say it. */
        if (stuck != nullptr)
            PANIC("Spinlock 0x%lX stuck, held %lu ns", (ulong)stuck, stuck_held_ns);

        for (ulong k = 0; k < late_count; k++)
            klog(0, "Spinlock 0x%p is held too long %lu", late[k], late_ns[k]);
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

    /* remove_init, not remove: the entry is left pointing at itself, which
       is what is_empty reads. A lock taken out of the watchdog's supervision
       (SpinLock::unwatch) is unregistered again by its destructor, and left
       linked to the neighbours it had it was unlinked a second time through
       them -- every lock registered between the two since cut out of the
       list, the count down twice, and a write through a neighbour that may
       have been destroyed in between. */
    ulong flags = list_lock.lock_irq_save();
    if (!lock.watchdog_list_entry_.is_empty())
    {
        lock.watchdog_list_entry_.remove_init();
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

        printer.printf("cpu %lu: %lu buckets, from %lu every %lu, moved on by one a tick\n", i,
            (ulong)((stdlib::array_size(spin_lock_list_) - cpu_slice_[i].first +
                cpu_slice_[i].stride - 1) / cpu_slice_[i].stride),
            cpu_slice_[i].first, cpu_slice_[i].stride);
    }
}

}