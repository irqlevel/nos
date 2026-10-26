#include "board.h"

#include <kernel/cpu.h>
#include <kernel/panic.h>
#include <kernel/stack_probe.h>
#include <kernel/parameters.h>
#include <kernel/trace.h>
#include <kernel/time.h>
#include <mm/page_table.h>
#include <mm/memory_map.h>
#include <lib/stdlib.h>

/* arm64 AP bring-up over PSCI CPU_ON (the x86 twin is the INIT/SIPI
   protocol in arch/x86_64/cpu_start.cpp). Defined as a per-arch TU of
   CpuTable so it keeps private access. */

namespace kernel
{

namespace
{

const u32 PSCI_CPU_ON = 0xC4000003;

/* PSCI return codes: 0 success. SMCCC allows the callee to clobber
   x4-x17 (QEMU's PSCI happens to preserve them; TF-A does not). */
#define SMCCC_CLOBBERS \
    "x4", "x5", "x6", "x7", "x8", "x9", "x10", "x11", \
    "x12", "x13", "x14", "x15", "x16", "x17", "memory"

long psci_call(u32 fn, ulong a1, ulong a2, ulong a3)
{
    register ulong x0 asm("x0") = fn;
    register ulong x1 asm("x1") = a1;
    register ulong x2 asm("x2") = a2;
    register ulong x3 asm("x3") = a3;

    if (Board::get_instance().psci_use_hvc_)
        asm volatile("hvc #0" : "+r"(x0) : "r"(x1), "r"(x2), "r"(x3)
            : SMCCC_CLOBBERS);
    else
        asm volatile("smc #0" : "+r"(x0) : "r"(x1), "r"(x2), "r"(x3)
            : SMCCC_CLOBBERS);
    return (long)x0;
}

/* Static boot stacks the APs run on until Cpu::run switches them to
   their idle-task stacks (the x86 twin: Stack[] in kernel/main.cpp). */
const ulong AP_BOOT_STACK_SIZE = 4 * consts::PAGE_SIZE;
char ap_boot_stack[MAX_CPUS][4 * consts::PAGE_SIZE]
    __attribute__((aligned(16)));

/* The MMU-off secondary entry reads these; clean them to the point of
   coherency so the uncached reads see the values. */
void clean_dcache_line(const void* p)
{
    asm volatile("dc civac, %0" :: "r"(p) : "memory");
}

}

extern "C" char secondary_entry[];
extern "C" ulong arm64_ap_ttbr1;
extern "C" ulong arm64_ap_stack_top[64];
/* sizeof arithmetic rather than stdlib::array_size: that helper is not
   constexpr, so it cannot be used in a static_assert. */
static_assert(sizeof(arm64_ap_stack_top) / sizeof(arm64_ap_stack_top[0]) == MAX_CPUS,
    "arm64_ap_stack_top (boot.S) must hold one entry per CPU");
static_assert(Board::MAX_BOARD_CPUS <= MAX_CPUS,
    "the FDT CPU table must not outgrow the kernel's CPU cap");

/* boot.S */
extern "C" char boot_stack_guard[];
extern "C" char boot_stack[];
extern "C" char boot_stack_top[];

void report_cpu_stacks(stdlib::Printer& printer, ulong& worst_free)
{
    /* The BSP's boot stack carries all of main_arm64 -- the deepest code on
       this arch -- and the AP stacks carry bring-up until Cpu::run moves each
       one onto an idle-task stack. None of them has the magic word or the
       one-page tripwire a task stack gets. */
    struct { const char* name; char* base; ulong size; ulong id; } stacks[MAX_CPUS + 1];
    ulong count = 0;

    stacks[count].name = "boot";
    stacks[count].base = &boot_stack[0];
    stacks[count].size = (ulong)&boot_stack_top[0] - (ulong)&boot_stack[0];
    stacks[count].id = 0;
    count++;

    for (ulong i = 0; i < MAX_CPUS && count < stdlib::array_size(stacks); i++)
    {
        stacks[count].name = "cpu-boot";
        stacks[count].base = &ap_boot_stack[i][0];
        stacks[count].size = AP_BOOT_STACK_SIZE;
        stacks[count].id = i;
        count++;
    }

    for (ulong i = 0; i < count; i++)
    {
        ulong free = stack_probe::untouched(stacks[i].base, stacks[i].size);
        if (free == stacks[i].size)
            continue;

        printer.printf("static %lu %lu %lu %s\n", stacks[i].id,
            stacks[i].size - free, stacks[i].size, stacks[i].name);

        if (free < worst_free)
            worst_free = free;
    }
}

bool CpuTable::start_all()
{
    auto& board = Board::get_instance();

    ulong entry_phys = (ulong)&secondary_entry[0] - mm::MemoryMap::KERNEL_SPACE_BASE;

    trace(0, "Starting cpus, entry 0x%lX", entry_phys);

    /* The boot stack runs down into this guard, and past it into the
       identity tables every AP walks as it turns its MMU on (boot.S). A
       guard that has been written in means the stack may have gone further:
       an AP started now could take a translation fault with no vector to go
       to, and the only sign of it would be "still not running". */
    ulong guard_size = (ulong)&boot_stack[0] - (ulong)&boot_stack_guard[0];
    if (stack_probe::untouched(&boot_stack_guard[0], guard_size) != guard_size)
        PANIC("the boot stack ran off its end, into the guard above the APs' identity tables");

    arm64_ap_ttbr1 = mm::PageTable::get_instance().get_root();
    for (ulong i = 0; i < stdlib::array_size(arm64_ap_stack_top) && i < MAX_CPUS; i++)
    {
        /* Filled before any AP is on it, so what survives is what it never
           reached. */
        stack_probe::poison(&ap_boot_stack[i][0], AP_BOOT_STACK_SIZE);
        arm64_ap_stack_top[i] = (ulong)&ap_boot_stack[i][AP_BOOT_STACK_SIZE - 16];
    }

    clean_dcache_line(&arm64_ap_ttbr1);
    for (ulong i = 0; i < stdlib::array_size(arm64_ap_stack_top); i++)
        clean_dcache_line(&arm64_ap_stack_top[i]);
    asm volatile("dsb sy" ::: "memory");

    const ulong bsp_index = get_bsp_index();
    const ulong max_cpus = Parameters::get_instance().get_max_cpus();
    /* Only the CPUs CPU_ON actually accepted are waited for below: with
       maxcpus=N the rest stay parked in the firmware and never report in. */
    ulong started_mask = 0;
    ulong running = 1; /* the BSP */

    for (ulong index = 0; index < MAX_CPUS; index++)
    {
        auto& cpu = get_cpu(index);

        if (index == bsp_index || !(cpu.get_state() & Cpu::STATE_INITED))
            continue;

        if (index >= board.cpu_count_)
            continue;

        if (max_cpus != 0 && running >= max_cpus)
        {
            trace(0, "Cpu %lu left parked, maxcpus is %lu", index, max_cpus);
            continue;
        }

        long err = psci_call(PSCI_CPU_ON, board.cpu_mpidr_[index],
            entry_phys, index);
        if (err != 0)
        {
            trace(0, "Cpu %lu CPU_ON failed %ld", index, err);
            return false;
        }

        started_mask |= 1UL << index;
        running++;
    }

    /* Poll for the APs to finish startup (mirrors x86): the budget scales
       with the number of them, because each one contends with the others on
       the dmesg lock and the heap on its way up. */
    static const ulong AP_TIMEOUT_BASE_MS = 500;
    static const ulong AP_TIMEOUT_PER_CPU_MS = 250;
    static const ulong AP_POLL_INTERVAL_MS = 10;

    const ulong ap_timeout_ms = AP_TIMEOUT_BASE_MS + AP_TIMEOUT_PER_CPU_MS * (running - 1);

    for (ulong waited = 0; waited < ap_timeout_ms; waited += AP_POLL_INTERVAL_MS)
    {
        busy_wait(AP_POLL_INTERVAL_MS * consts::NANO_SECS_IN_MS);

        bool all_running = true;
        for (ulong index = 0; index < MAX_CPUS; index++)
        {
            if (!(started_mask & (1UL << index)))
                continue;

            if (!(get_cpu(index).get_state() & Cpu::STATE_RUNNING))
            {
                all_running = false;
                break;
            }
        }
        if (all_running)
            break;
    }

    for (ulong index = 0; index < MAX_CPUS; index++)
    {
        if (!(started_mask & (1UL << index)))
            continue;

        if (!(get_cpu(index).get_state() & Cpu::STATE_RUNNING))
        {
            trace(0, "Cpu %lu still not running after %lu ms",
                index, ap_timeout_ms);
            return false;
        }
    }

    trace(0, "Cpus started, %lu running", running);

    return true;
}

}
