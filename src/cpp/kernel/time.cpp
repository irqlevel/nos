#include "time.h"
#include <arch/x86_64/tsc.h>
#include "trace.h"
#include <hal/cpu.h>

#include <drivers/pit.h>
#include <drivers/hpet.h>
#include <drivers/rtc.h>

namespace kernel
{

enum ClockSource
{
    ClockPit = 0,
    ClockTsc,
    ClockKvmClock,
};

static ClockSource active_source = ClockPit;
static ulong rtc_epoch_secs = 0;

/* Added to the active source's reading so get_boot_time() stays one
   continuous timeline across a source switch (see set_clock_source).
   The HPET/PIT fallback below defines that timeline and takes no
   offset. */
static long clock_offset_ns = 0;

/* The timeline get_boot_time() reports: the HPET the kernel reset in
   Hpet::setup(), or the PIT when there is no HPET. It runs from before
   the first get_boot_time() call to the last, so it is the one clock every
   other source is aligned onto and it never takes an offset. */
static stdlib::Time read_base_clock()
{
    if (Hpet::get_instance().is_available())
        return Hpet::get_instance().get_time();

    return Pit::get_instance().get_time();
}

/* Reads the active source, or the base clock when that source has
   nothing to say. *primary reports which one answered, because only the
   active source needs the offset. */
static stdlib::Time read_clock_source(bool* primary)
{
    *primary = true;

    switch (active_source)
    {
    case ClockKvmClock:
    case ClockTsc:
    {
        /* Ask for the source this offset was computed against, not for
           Tsc::get_time()'s "best" one: that one silently becomes kvmclock
           the moment setup_kvm_clock() arms it, which would move a ClockTsc
           reading onto a raw clock with a different zero. */
        stdlib::Time t = (active_source == ClockKvmClock)
            ? Tsc::get_instance().get_time()
            : Tsc::get_instance().tsc_time();
        if (t.get_value() != 0)
            return t;
        /* Fallthrough to the base clock if TSC returns 0 (shouldn't happen) */
        break;
    }
    default:
        break;
    }

    *primary = false;

    return read_base_clock();
}

/* Point get_boot_time() at another source without moving the clock.

   Every source has its own zero: the HPET main counter is reset in
   Hpet::setup(), the TSC baseline is taken here in time_init(), and
   kvmclock counts from VM start. Switching to one raw therefore steps
   the reported time -- back by everything the kernel did between the
   two zeros (the self test alone runs tens of seconds), or forward
   onto a clock that was already running before the kernel was.

   Whatever holds an absolute deadline across the step then waits it
   out. The 8042 timer that decodes scancodes is armed from
   Interrupt::register_handler, before time_init, at now + 10ms: step the clock
   back and nothing decodes a keystroke until the new clock passes that
   mark, so keys pile up in the driver's ring buffer and all land at
   once, half a minute later. Stepping forward instead makes every lock
   the watchdog is timing look held for the size of the step.

   The two readings are taken from the new source and from the base
   clock, never from get_boot_time(): enabling a source can change what the
   *current* source reads before active_source moves -- setup_kvm_clock()
   flips Tsc::get_time() over to kvmclock -- so a "before" taken through
   get_boot_time() would already be on the new raw clock carrying the old
   offset. That measures a zero step, keeps the stale offset and doubles
   the reported boot time.

   IRQs are off because process_timers reads the clock from the tick IPI
   on this CPU; the APs are not started yet, so there is no other
   reader. */
static void set_clock_source(ClockSource source)
{
    ulong flags = hal::irq_save();

    active_source = source;
    clock_offset_ns = 0;

    bool primary = false;
    ulong raw = read_clock_source(&primary).get_value();
    if (primary)
        clock_offset_ns = (long)read_base_clock().get_value() - (long)raw;

    hal::irq_restore(flags);
}

void time_init()
{
    auto& tsc = Tsc::get_instance();

    /* Always calibrate TSC if possible */
    if (tsc.calibrate())
    {
        if (tsc.is_invariant())
            set_clock_source(ClockTsc);
    }
    else
    {
        trace(0, "Time: TSC calibration failed");
    }

    /* Try kvmclock (takes priority over TSC for get_boot_time) */
    if (tsc.setup_kvm_clock())
    {
        set_clock_source(ClockKvmClock);
        trace(0, "Time: using kvmclock");
    }
    else if (active_source == ClockTsc)
    {
        trace(0, "Time: using calibrated TSC");
    }
    else
    {
        trace(0, "Time: using PIT fallback");
    }

    /* Read RTC wall clock. Store the epoch of boot time zero, not of
       now: get_boot_time() already counts the stretch before time_init, and
       get_wall_time_secs adds the two. */
    RtcTime rtc;
    if (Rtc::get_instance().read_time(rtc))
    {
        rtc_epoch_secs = Rtc::to_unix_epoch(rtc) - get_boot_time().get_secs();
        trace(0, "Time: RTC epoch %lu", rtc_epoch_secs);
    }

    /* Counts from the kernel's own start, not from this line: the
       restart-at-zero this replaces hid how long the self test and the
       device probe really take. */
    trace(0, "Time: boot time %lu ns", get_boot_time().get_value());
}

stdlib::Time get_boot_time()
{
    bool primary = false;
    ulong raw = read_clock_source(&primary).get_value();

    if (!primary)
        return stdlib::Time(raw);

    long offset = clock_offset_ns;

    if (offset >= 0)
        return stdlib::Time(raw + (ulong)offset);

    ulong back = (ulong)(-offset);

    return stdlib::Time((raw > back) ? (raw - back) : 0);
}

ulong cycle_counter_delta_to_ns(u64 delta)
{
    /* hal::read_cycle_counter is the TSC here; tsc_to_ns returns 0 until
       calibrate() has run. */
    return Tsc::get_instance().tsc_to_ns(delta);
}

void busy_wait(ulong nano_secs)
{
    stdlib::Time expired = get_boot_time() + stdlib::Time(nano_secs);
    while (get_boot_time() < expired)
    {
        cpu_relax();
    }
}

ulong get_wall_time_secs()
{
    return rtc_epoch_secs + get_boot_time().get_secs();
}

}
