#include <hal/console.h>
#include <hal/pmu.h>
#include <hal/power.h>
#include <hal/cpu.h>
#include <hal/irqchip.h>
#include <hal/mmu.h>
#include <hal/module.h>

#include <kernel/elf.h>
#include <lib/printer.h>

#include "pl011.h"
#include "board.h"

/* arm64 backends for the HAL console and power services (x86 twin:
   arch/x86_64/hal_x86.cpp). PL011 output is polled in both paths for now;
   the interrupt-driven console arrives with the GIC milestone. */

namespace
{

/* PSCI 0.2 function IDs */
const u32 PSCI_SYSTEM_OFF = 0x84000008;
const u32 PSCI_SYSTEM_RESET = 0x84000009;

/* SMCCC: the callee may clobber x1-x17 */
#define SMCCC_CLOBBERS \
    "x1", "x2", "x3", "x4", "x5", "x6", "x7", "x8", "x9", "x10", "x11", \
    "x12", "x13", "x14", "x15", "x16", "x17", "memory"

void psci_call(u32 fn)
{
    register ulong x0 asm("x0") = fn;
    if (kernel::Board::get_instance().psci_use_hvc_)
        asm volatile("hvc #0" : "+r"(x0) :: SMCCC_CLOBBERS);
    else
        asm volatile("smc #0" : "+r"(x0) :: SMCCC_CLOBBERS);
}

}

namespace hal
{

bool setup_serializing_lfence()
{
    /* Nothing to set: an ISB orders what follows it on every arm64 CPU. */
    return false;
}

bool setup_cpu_performance(CpuPerformance* out)
{
    /* The clock is firmware's to set here, through PSCI and SCMI. */
    *out = CpuPerformance{};
    return false;
}

ulong cycle_counter_hz()
{
    /* The generic timer's rate, which firmware sets and nothing changes:
       what read_cycle_counter's CNTVCT_EL0 counts at. */
    u64 freq;
    asm volatile("mrs %0, cntfrq_el0" : "=r"(freq));
    return freq;
}

/* PMUv3 exists on the hardware but is not wired up here, and Apple's
   hypervisor does not hand the guest one to test against. */
bool pmu_available()
{
    return false;
}

bool pmu_start()
{
    return false;
}

void pmu_stop()
{
}

const char* pmu_name()
{
    return "none";
}

/* Who made the core, by MIDR_EL1's implementer field */
static const char* implementer_name(ulong implementer)
{
    switch (implementer)
    {
    case 0x41: return "ARM";
    case 0x42: return "Broadcom";
    case 0x43: return "Cavium";
    case 0x4E: return "NVIDIA";
    case 0x50: return "APM";
    case 0x51: return "Qualcomm";
    case 0x61: return "Apple";
    case 0x00: return "reserved/emulated";
    default: return "unknown";
    }
}

void cpu_model(char* buf, unsigned long len)
{
    if (buf == nullptr || len == 0)
        return;

    ulong midr;
    asm volatile("mrs %0, midr_el1" : "=r"(midr));
    stdlib::sn_printf(buf, len, "%s part 0x%lX r%lup%lu", implementer_name((midr >> 24) & 0xFF),
        (midr >> 4) & 0xFFF, (midr >> 20) & 0xF, midr & 0xF);
}

void print_cpu_info(stdlib::Printer& con)
{
    ulong midr, ctr, mmfr0;
    asm volatile("mrs %0, midr_el1" : "=r"(midr));
    asm volatile("mrs %0, ctr_el0" : "=r"(ctr));
    asm volatile("mrs %0, id_aa64mmfr0_el1" : "=r"(mmfr0));

    ulong implementer = (midr >> 24) & 0xFF;
    const char* name = implementer_name(implementer);

    con.printf("implementer 0x%lX (%s)\n", implementer, name);
    con.printf("part 0x%lX variant %lu revision %lu, midr 0x%lX\n",
        (midr >> 4) & 0xFFF, (midr >> 20) & 0xF, midr & 0xF, midr);

    /* Log2 of the words in the smallest cache line, per CTR_EL0. */
    con.printf("cache line: i %lu bytes, d %lu bytes\n",
        4UL << (ctr & 0xF), 4UL << ((ctr >> 16) & 0xF));

    /* PARange, the physical address width this core can address. */
    static const u8 PARANGE_BITS[] = { 32, 36, 40, 42, 44, 48, 52, 56 };
    ulong parange = mmfr0 & 0xF;
    con.printf("physical address bits %lu\n",
        (parange < stdlib::array_size(PARANGE_BITS))
            ? (ulong)PARANGE_BITS[parange] : 0UL);
}

void print_cpu_state(stdlib::Printer& con)
{
    ulong v;
    asm volatile("mrs %0, CurrentEL" : "=r"(v));
    con.printf("el %lu sp 0x%lX", v >> 2, hal::get_sp());
    asm volatile("mrs %0, sctlr_el1" : "=r"(v));
    con.printf(" sctlr 0x%lX", v);
    asm volatile("mrs %0, tcr_el1" : "=r"(v));
    con.printf(" tcr 0x%lX\n", v);
    asm volatile("mrs %0, ttbr1_el1" : "=r"(v));
    con.printf("ttbr1 0x%lX", v);
    asm volatile("mrs %0, mair_el1" : "=r"(v));
    con.printf(" mair 0x%lX", v);
    asm volatile("mrs %0, daif" : "=r"(v));
    con.printf(" daif 0x%lX", v);
    asm volatile("mrs %0, vbar_el1" : "=r"(v));
    con.printf(" vbar 0x%lX mpidr %lu\n", v, hal::get_current_cpu_hw_id());
}

void enable_wx_support()
{
    /* PXN/UXN are always active on arm64 — nothing to enable. */
}

void sync_instruction_cache(ulong va, ulong size)
{
    if (size == 0)
        return;

    /* CTR_EL0 DminLine [19:16] and IminLine [3:0]: log2 of the smallest
       data and instruction cache line, in 4-byte words */
    ulong ctr;
    asm volatile("mrs %0, ctr_el0" : "=r"(ctr));
    const ulong dline = 4UL << ((ctr >> 16) & 0xF);
    const ulong iline = 4UL << (ctr & 0xF);
    const ulong end = va + size;

    /* Instruction fetch does not snoop the data cache: push the freshly
       written code out to the point of unification, where fetch sees it... */
    for (ulong p = va & ~(dline - 1); p < end; p += dline)
        asm volatile("dc cvau, %0" :: "r"(p) : "memory");
    asm volatile("dsb ish" ::: "memory");

    /* ...then drop whatever any CPU's instruction cache still holds for these
       lines -- IC IVAU is broadcast to the inner-shareable domain -- and make
       this CPU refetch */
    for (ulong p = va & ~(iline - 1); p < end; p += iline)
        asm volatile("ic ivau, %0" :: "r"(p) : "memory");
    asm volatile("dsb ish" ::: "memory");
    asm volatile("isb" ::: "memory");
}

u16 module_elf_machine()
{
    return kernel::elf::MACHINE_AARCH64;
}

ModuleReloc classify_module_reloc(u32 type)
{
    /* AArch64 ELF ABI numbering */
    static const u32 REL_NONE = 0;
    static const u32 REL_ABS64 = 257;     /* R_AARCH64_ABS64: a pointer in data */
    static const u32 REL_GLOB_DAT = 1025;  /* R_AARCH64_GLOB_DAT: a GOT slot */
    static const u32 REL_JUMP_SLOT = 1026; /* R_AARCH64_JUMP_SLOT: a PLT's slot */
    static const u32 REL_RELATIVE = 1027; /* R_AARCH64_RELATIVE */

    switch (type)
    {
    case REL_NONE:
        return ModuleReloc::None;
    case REL_RELATIVE:
        return ModuleReloc::Relative;
    case REL_ABS64:
    case REL_GLOB_DAT:
    case REL_JUMP_SLOT:
        return ModuleReloc::Symbol;
    default:
        return ModuleReloc::Unsupported;
    }
}

void setup_memory_types()
{
    /* boot.S loads the same MAIR_EL1 on the boot CPU and on every
       secondary, and idx2 is already Normal non-cacheable — the arm64
       equivalent of write-combining. Nothing to do per CPU. */
}

bool is_write_combining_available()
{
    return true;
}

void console_out(const char *s)
{
    kernel::Pl011::print_string(s);
}

void console_out_backspace()
{
    kernel::Pl011::print_string("\b \b");
}

void console_out_clear()
{
    /* ANSI escape: clear screen and move cursor home */
    kernel::Pl011::print_string("\033[2J\033[H");
}

void console_write(const char *msg)
{
    kernel::Pl011::print_string(msg);
}

void console_panic_write(const char *msg)
{
    kernel::Pl011::panic_print_string(msg);
}

void power_off()
{
    psci_call(PSCI_SYSTEM_OFF);
    for (;;)
    {
        asm volatile("wfi");
    }
}

void reset()
{
    reset_now();
}

void reset_now()
{
    psci_call(PSCI_SYSTEM_RESET);
    for (;;)
    {
        asm volatile("wfi");
    }
}

}
