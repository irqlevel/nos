#pragma once

#include <include/types.h>
#include <lib/printer.h>
#include <drivers/acpi.h>
#include <kernel/spin_lock.h>

namespace kernel
{

/* Intel VT-d (Intel Virtualization Technology for Directed I/O,
   Architecture Specification rev. 4.1): the DMA remapping units the DMAR
   names, each a block of registers between the devices behind it and
   memory. A unit is driven in legacy mode -- a root table with an entry a
   bus, a context table a bus with an entry a device and function -- and
   with queued invalidation; nothing of scalable mode, PASIDs or nested
   translation.

   With iommu=on each unit that translates a device of nos's is taken over:
   translation on, every device passed through (TT=10b), its DMA going to
   the physical address it names as with no IOMMU, so that no driver
   changes. What that buys is the place a device given to a guest is then
   moved to -- a domain of its own -- and every fault a unit records said
   where it happens. A unit whose devices are all display devices, which nos
   drives none of, is left as the firmware left it. */
class Vtd final
{
public:
    static Vtd& get_instance()
    {
        static Vtd instance;
        return instance;
    }

    /* Once, after the PCI scan and before a driver starts a DMA or the first
       interrupt is programmed: each unit's registers mapped and read. With
       iommu=on the units are taken over as above, all of them or none: a
       DMAR not kept whole, a unit that cannot be driven -- no queued
       invalidation, no pass-through, registers past what its DRHD maps -- or
       one that does not come up leaves every unit as it was, which is said.
       Either way, what the firmware left on that nos does not drive --
       translation, protected memory regions, interrupt remapping -- is
       turned off and said: a DMA through tables the firmware is done with,
       or into a region it protected, would fail with no word. */
    void setup();

    /* Whether DMA is translated: iommu=on, and every unit came up */
    bool is_on() const;

    /* `iommu`: each unit, what it can do, what it does, its faults -- each
       unit's records looked at first. `iommu block <bus>:<dev>.<fn>` makes a
       device's context entry not present in every unit taken over, its DMA
       then blocked and said as a device detached from a domain's is;
       `iommu unblock` passes it through again: for a gate to see a blocked
       DMA said, and the machine go on. */
    void shell(const char* args, stdlib::Printer& printer);

    /* The fault event's handler, in interrupt context: each unit's recorded
       faults said and cleared, so many a second at most, and a unit's
       interrupt masked once its faults are a storm */
    void on_fault_interrupt();

    /* The vector every unit's fault event is sent at, to the boot CPU:
       above every device's (MsixTable's) and below the posted interrupts'
       (hal::POSTED_VECTOR) */
    static const u8 FAULT_VECTOR = 0xFB;

private:
    Vtd();
    /* Nothing is torn down at the halt: a unit translating DMA keeps its
       tables for as long as the machine runs */
    ~Vtd();
    Vtd(const Vtd& other) = delete;
    Vtd(Vtd&& other) = delete;
    Vtd& operator=(const Vtd& other) = delete;
    Vtd& operator=(Vtd&& other) = delete;

    static const ulong TAG = 'Vtd ';

    /* What a legacy-mode table has room for: a bus a root entry, a device
       and function a context entry */
    static const size_t BUSES = 256;
    static const size_t DEVFNS = 256;

    struct Unit
    {
        const Acpi::DmarUnit* dmar;
        u8 index;
        ulong phys;
        /* Bytes of its registers mapped: 2^size pages, as its DRHD says */
        ulong span;
        volatile u8* regs;
        u32 version;
        u64 cap;
        u64 ecap;
        /* Every device it names a display device: left be */
        bool graphics;
        /* Taken over: translating, on the tables below */
        bool taken;
        /* The context entries' AW: the widest table it walks */
        u8 width_code;
        u64* root;
        ulong root_phys;
        /* A context table a bus, for buses 0 to last_bus_ */
        u64* context[BUSES];
        ulong context_phys[BUSES];
        /* The invalidation queue: a page of descriptors of 16 bytes */
        u64* queue;
        ulong queue_phys;
        ulong queue_tail;
        /* The word an invalidation wait descriptor writes, in a page of
           its own, and the value the next wait is to write */
        volatile u32* status;
        ulong status_phys;
        u32 status_seq;
        /* One submitter at a time: a descriptor and its wait, waited for */
        SpinLock queue_lock;
        /* An invalidation that failed or was not done: the queue's head is
           then where the unit stopped, and a tail that went on would come
           round onto descriptors it never took -- nothing more is queued */
        bool broken;
        /* Its faults: said, recorded past the rate and not said, the
           second the rate is counted in and how many it has had, and
           whether its interrupt was masked for a storm */
        ulong faults;
        ulong faults_unsaid;
        ulong rate_second;
        ulong rate_count;
        bool storm;
        /* Its fault event is this CPU's interrupt */
        bool fault_irq;
        const char* state;
    };

    /* The unit's registers */
    u32 read32(const Unit& unit, ulong offset) const;
    u64 read64(const Unit& unit, ulong offset) const;
    void write32(Unit& unit, ulong offset, u32 value);
    void write64(Unit& unit, ulong offset, u64 value);

    /* GCMD written whole -- the states GSTS says are on, with bit set or
       cleared -- and GSTS waited on to say so; false, said, where it does
       not within WAIT_NS. A one-shot command (setting the root table's
       pointer) is set, and its status waited on to come. */
    bool command(Unit& unit, u32 bit, bool on);

    /* Whether read32(offset) & mask comes to be value within WAIT_NS */
    bool wait_register(Unit& unit, ulong offset, u32 mask, u32 value);

    /* The write buffer flushed, for a unit that asks for it (CAP.RWBF)
       before a change of its tables is to be seen */
    bool flush_write_buffer(Unit& unit);

    /* The descriptor lo/hi, and a wait behind it, through the queue, and
       the wait waited for: false, said, on a queue error or a unit that
       does not answer within WAIT_NS */
    bool invalidate(Unit& unit, u64 lo, u64 hi);

    /* Write the lines of [va, va + bytes) back to memory where the unit
       does not snoop the caches when it reads its tables (ECAP.C clear) */
    void flush(const Unit& unit, const void* va, ulong bytes) const;

    /* The unit's registers mapped, read, and what it is decided: false,
       said, where it cannot be driven */
    bool probe(Unit& unit);

    /* What the firmware left on that nos does not drive, turned off */
    void put_right(Unit& unit);

    /* Its tables made, the queue up, its fault event this CPU's,
       translation on: false, said, with nothing of it left on */
    bool take_over(Unit& unit);

    /* A unit taken over, given back: translation and the queue off, its
       tables freed */
    void release(Unit& unit);

    /* Whether every device the unit names is a display device */
    bool names_display_alone(const Acpi::DmarUnit& dmar);

    /* The bus, device and function a scope's path ends at, through the
       bridges' secondary buses: false where a hop's bridge is not there */
    static bool resolve(const Acpi::DmarScope& scope, u8& bus, u8& dev, u8& fn);

    /* The highest bus a device found by the scan is on or a bridge leads
       to: the buses the context tables are made for */
    static u8 last_bus();

    /* The unit's recorded faults said and cleared */
    void report_faults(Unit& unit);

    /* A fault's reason, as the spec names it (7.1.3, 7.1.4) */
    static const char* fault_reason(u8 reason);

    void free_tables(Unit& unit);

    void print(stdlib::Printer& printer);

    /* The device's context entry in every unit taken over: not present, or
       passed through; each unit's caches of it dropped. False, said, where
       nothing translates or its bus has no context table. */
    bool set_blocked(u8 bus, u8 dev, u8 fn, bool blocked, stdlib::Printer& printer);

    /* `<bus>:<dev>.<fn>` in hex, as `pci` and the faults name a device */
    static bool parse_bdf(const char* s, u8& bus, u8& dev, u8& fn);

    /* How long a unit has to answer a command or an invalidation: it takes
       microseconds, and one that takes a second is broken */
    static const ulong WAIT_NS = 1000UL * 1000 * 1000;
    static const ulong WAIT_STEP_NS = 1000;

    /* What is said of faults: so many lines a second a unit, and past so
       many faults a second its interrupt masked (a device that DMAs where
       it may not as fast as it can would hold the boot CPU in this
       handler) -- `iommu` looks at it again and unmasks it */
    static const ulong FAULTS_SAID_A_SECOND = 16;
    static const ulong FAULT_STORM = 4096;

    Unit* units_[Acpi::MAX_DMAR_UNITS];
    size_t unit_count_;
    /* The units' fault records and counts, read from the fault event's
       interrupt and from `iommu`: one at a time, interrupts off */
    SpinLock fault_lock_;
    u8 last_bus_;
    bool on_;
    /* The APIC id of the CPU the fault events go to */
    u32 fault_apic_id_;
};

}
