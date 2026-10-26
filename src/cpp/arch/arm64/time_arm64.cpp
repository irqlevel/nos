#include <kernel/time.h>
#include <kernel/trace.h>
#include <mm/memory_map.h>

#include "board.h"

/* arm64 implementation of the kernel/time.h API over the ARM generic
   timer: CNTVCT_EL0 at CNTFRQ_EL0 Hz, architecturally specified — no
   calibration needed (x86 twin: kernel/time.cpp + arch/x86_64/tsc.cpp).
   Wall clock arrives with the PL031 driver (milestone M3). */

namespace kernel
{

namespace
{

u64 boot_count;
u64 freq_hz;
ulong boot_epoch_secs;

u64 read_cntvct()
{
    u64 cnt;
    asm volatile("isb; mrs %0, cntvct_el0" : "=r"(cnt));
    return cnt;
}

u64 read_cntfrq()
{
    u64 freq;
    asm volatile("mrs %0, cntfrq_el0" : "=r"(freq));
    return freq;
}

u64 count_to_ns(u64 count)
{
    /* Split to avoid 128-bit division (no compiler-rt in the kernel):
       remainder * 1e9 stays < 2^63 for any sane counter frequency. */
    return (count / freq_hz) * consts::NANO_SECS_IN_SEC +
           ((count % freq_hz) * consts::NANO_SECS_IN_SEC) / freq_hz;
}

}

void time_init()
{
    if (freq_hz == 0)
    {
        /* Not yet self-armed by an early GetBootTime call */
        freq_hz = read_cntfrq();
        boot_count = read_cntvct();
    }
    trace(0, "TimeInit: generic timer freq %lu Hz", freq_hz);

    /* PL031 RTC: seconds since the Unix epoch (QEMU virt), through the
       premapped device GiB. The x86 twin reads the CMOS RTC. */
    ulong pl031 = Board::get_instance().pl031_base_;
    if (pl031 != 0)
    {
        u32 now = *reinterpret_cast<volatile u32*>(
            mm::MemoryMap::KERNEL_SPACE_BASE + pl031);
        ulong booted_secs = count_to_ns(read_cntvct() - boot_count) /
            consts::NANO_SECS_IN_SEC;
        boot_epoch_secs = now - booted_secs;
        trace(0, "TimeInit: rtc epoch %lu", (ulong)now);
    }
}

stdlib::Time get_boot_time()
{
    if (freq_hz == 0)
    {
        /* Callable before TimeInit (locks trace boot time); self-arm */
        freq_hz = read_cntfrq();
        boot_count = read_cntvct();
    }

    stdlib::Time time;
    time.nano_secs = count_to_ns(read_cntvct() - boot_count);
    return time;
}

ulong cycle_counter_delta_to_ns(u64 delta)
{
    /* Hal::ReadCycleCounter is CNTVCT_EL0, the same counter GetBootTime
       reads, so FreqHz converts it directly. */
    if (freq_hz == 0)
        return 0;

    return count_to_ns(delta);
}

void busy_wait(ulong nano_secs)
{
    if (freq_hz == 0)
        get_boot_time();

    u64 target = read_cntvct() +
        (nano_secs / consts::NANO_SECS_IN_SEC) * freq_hz +
        ((nano_secs % consts::NANO_SECS_IN_SEC) * freq_hz) / consts::NANO_SECS_IN_SEC;
    while (read_cntvct() < target)
    {
        asm volatile("yield");
    }
}

ulong get_wall_time_secs()
{
    return boot_epoch_secs + get_boot_time().nano_secs / consts::NANO_SECS_IN_SEC;
}

}
