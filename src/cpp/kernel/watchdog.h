#pragma once

#include <lib/stdlib.h>
#include <lib/list_entry.h>
#include <lib/printer.h>

#include "raw_spin_lock.h"
#include "atomic.h"

namespace kernel
{

class Watchdog final
{
public:
    static Watchdog& get_instance()
    {
        static Watchdog instance;
        return instance;
    }

    void register_spin_lock(RawSpinLock& lock);
    void unregister_spin_lock(RawSpinLock& lock);

    void check();

    void dump(stdlib::Printer& printer);

    /* Checked against MaxCpus in the .cpp; kept here so this header does not
       have to pull in cpu.h. */
    static const ulong SLICE_CPUS = 64;

private:
    Watchdog(const Watchdog& other) = delete;
    Watchdog(Watchdog&& other) = delete;
    Watchdog& operator=(const Watchdog& other) = delete;
    Watchdog& operator=(Watchdog&& other) = delete;


    static const size_t SPIN_LOCK_HASH_SIZE = 512;

    /* The watchdog's own locks cannot be RawSpinLocks: a RawSpinLock
       registers itself with the watchdog when it is constructed, and these
       are constructed as part of constructing the watchdog. Same two
       instructions, no registration. */
    class ListLock final
    {
    public:
        ulong lock_irq_save();
        void unlock_irq_restore(ulong flags);

    private:
        Atomic value_;
    };

    stdlib::ListEntry spin_lock_list_[SPIN_LOCK_HASH_SIZE];
    ListLock spin_lock_list_lock_[SPIN_LOCK_HASH_SIZE];

    /* The slice of the bucket table each CPU walks, for Dump. */
    struct Slice
    {
        ulong first;
        ulong stride;
    };

    Slice cpu_slice_[SLICE_CPUS];

    Atomic check_counter_;
    Atomic spin_lock_counter_;

    Watchdog();
    ~Watchdog();
};

}