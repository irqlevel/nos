#include <kernel/cpu.h>
#include <kernel/parameters.h>
#include <kernel/time.h>
#include <kernel/trace.h>
#include <arch/x86_64/lapic.h>
#include <arch/x86_64/boot64.h>

#include <lib/stdlib.h>

namespace kernel
{

/* The x86 MP startup protocol (INIT + 2x SIPI + poll) for CpuTable.
   Defined in an arch TU so another architecture can supply its own
   CpuTable::StartAll (e.g. PSCI CPU_ON) with full private access. */

namespace
{

/* Bring one AP up and wait for it to report in.

   One CPU at a time, rather than INIT-ing all of them and then waiting for
   the crowd: the trace line below names the CPU that is being poked, so a
   machine that dies during bring-up says which one it choked on, and the APs
   no longer race each other through the dmesg lock, the heap and the shared
   GDT on their way up. The cost is the 10ms INIT delay per CPU -- the polling
   ends as soon as that one AP reports running. */
bool start_cpu(Cpu& cpu, ulong index, ulong startup_vector)
{
    static const ulong INIT_DELAY_MS = 10;   /* 10ms after INIT */
    static const ulong SIPI_RETRIES = 2;    /* the first SIPI can be lost */
    static const ulong SIPI_DELAY_US = 200;  /* Intel MP spec: 200us apart */
    static const ulong AP_TIMEOUT_MS = 1000;
    static const ulong AP_POLL_INTERVAL_MS = 1;

    trace(0, "Cpu %lu: INIT+SIPI vector 0x%lX", index, startup_vector);

    Lapic::send_init(index);
    busy_wait(INIT_DELAY_MS * consts::NANO_SECS_IN_MS);

    for (ulong sipi = 0; sipi < SIPI_RETRIES; sipi++)
    {
        if (cpu.get_state() & Cpu::STATE_RUNNING)
            return true;

        Lapic::send_startup(index, startup_vector);
        busy_wait(SIPI_DELAY_US * consts::NANO_SECS_IN_USEC);
    }

    for (ulong waited = 0; waited < AP_TIMEOUT_MS; waited += AP_POLL_INTERVAL_MS)
    {
        if (cpu.get_state() & Cpu::STATE_RUNNING)
            return true;

        busy_wait(AP_POLL_INTERVAL_MS * consts::NANO_SECS_IN_MS);
    }

    trace(0, "Cpu %lu still not running after %lu ms", index, AP_TIMEOUT_MS);

    return false;
}

}

bool CpuTable::start_all()
{
    ulong startup_code = (ulong)ap_start16;

    trace(0, "Starting cpus, startupCode 0x%lX", startup_code);

    if (startup_code & (consts::PAGE_SIZE - 1))
        return false;

    if (startup_code >= 0x100000)
        return false;

    const ulong bsp_index = get_bsp_index();
    const ulong max_cpus = Parameters::get_instance().get_max_cpus();
    ulong running = 1; /* the BSP */

    for (ulong index = 0; index < MAX_CPUS; index++)
    {
        auto& cpu = get_cpu(index);

        if (index == bsp_index || !(cpu.get_state() & Cpu::STATE_INITED))
            continue;

        if (max_cpus != 0 && running >= max_cpus)
        {
            trace(0, "Cpu %lu left parked, maxcpus is %lu", index, max_cpus);
            continue;
        }

        if (!start_cpu(cpu, index, startup_code >> consts::PAGE_SHIFT))
            return false;

        running++;
    }

    trace(0, "Cpus started, %lu running", running);

    return true;
}

}
