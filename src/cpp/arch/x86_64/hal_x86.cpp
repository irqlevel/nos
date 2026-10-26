#include <hal/console.h>
#include <hal/power.h>
#include <hal/cpu.h>
#include <hal/mmu.h>
#include <hal/module.h>
#include <hal/pmu.h>
#include <arch/x86_64/pmu.h>

#include <arch/x86_64/asm.h>
#include <arch/x86_64/cpuid.h>

#include <arch/x86_64/context.h>
#include <arch/x86_64/tsc.h>
#include <lib/stdlib.h>
#include <lib/printer.h>

#include <kernel/trace.h>
#include <kernel/parameters.h>
#include <kernel/elf.h>
#include <drivers/serial.h>
#include <drivers/screen.h>
#include <drivers/acpi.h>

namespace hal
{

void enable_wx_support()
{
    /* EFER (MSR 0xC0000080) bit 11 = NXE: honor the NX bit in PTEs. */
    static const u32 EFER_MSR = 0xC0000080;
    static const u64 EFER_NXE = 1ULL << 11;
    u64 efer = read_msr(EFER_MSR);
    if (!(efer & EFER_NXE))
        write_msr(EFER_MSR, efer | EFER_NXE);

    /* CR0.WP (bit 16): without it, ring-0 writes ignore the read-only PTE
       bit, so the kernel could still write .text. Enforce it for W^X. */
    asm volatile(
        "mov %%cr0, %%rax\n\t"
        "or $0x10000, %%rax\n\t"
        "mov %%rax, %%cr0\n\t"
        ::: "rax", "memory");
}

namespace
{

/* Set once setup_memory_types() has programmed the PAT; every CPU writes the
   same value, so the racy stores are benign. */
bool write_combining_ready;

bool cpu_has_pat()
{
    /* CPUID.01H:EDX[16] = PAT */
    u32 eax, ebx, ecx, edx;

    asm volatile("cpuid"
        : "=a"(eax), "=b"(ebx), "=c"(ecx), "=d"(edx)
        : "a"(1U), "c"(0U));

    return (edx & (1U << 16)) != 0;
}

}

void setup_memory_types()
{
    static const u32 PAT_MSR = 0x277;
    static const u32 WC_ENTRY = 4;      /* PTE bit PAT=1, PCD=0, PWT=0 */
    static const u64 PAT_TYPE_WC = 0x01;
    static const u64 PAT_ENTRY_MASK = 0xFF;

    if (!cpu_has_pat())
    {
        trace(0, "PAT: not supported, MMIO stays uncached");
        return;
    }

    u64 pat = read_msr(PAT_MSR);
    pat &= ~(PAT_ENTRY_MASK << (WC_ENTRY * 8));
    pat |= PAT_TYPE_WC << (WC_ENTRY * 8);

    /* Changing a memory type wants the caches flushed and the TLB clean
       (SDM 11.11.8). Interrupts are still off here on both the BSP and the
       APs, and entry 4 is not used by any existing mapping. */
    asm volatile("wbinvd" ::: "memory");
    write_msr(PAT_MSR, pat);
    asm volatile("wbinvd" ::: "memory");
    tlb_flush_all();

    write_combining_ready = true;

    trace(0, "PAT: 0x%lX, entry %lu = WC", (ulong)pat, (ulong)WC_ENTRY);
}

bool is_write_combining_available()
{
    return write_combining_ready;
}

ulong cycle_counter_hz()
{
    auto& tsc = kernel::Tsc::get_instance();
    return tsc.is_calibrated() ? tsc.get_freq_hz() : 0;
}

bool setup_serializing_lfence()
{
    static const u32 LEAF_VENDOR = 0;
    static const u32 LEAF_FEATURES = 1;
    static const u32 FEATURES_ECX_HYPERVISOR = 1U << 31;
    /* "AuthenticAMD" and "HygonGenuine", as EBX, EDX, ECX give them. */
    static const u32 AMD_EBX = 0x68747541, AMD_EDX = 0x69746E65, AMD_ECX = 0x444D4163;
    static const u32 HYGON_EBX = 0x6F677948, HYGON_EDX = 0x6E65476E, HYGON_ECX = 0x656E6975;
    static const u32 FAMILY_SHIFT = 8, FAMILY_MASK = 0xF;
    static const u32 EXT_FAMILY_SHIFT = 20, EXT_FAMILY_MASK = 0xFF;
    static const u32 FAMILY_EXTENDED = 0xF;
    /* The first family with the MSR, and the one after it without: 11h
       (Griffin) has no DE_CFG, and serializes LFENCE as 0Fh does. */
    static const u32 FAMILY_FIRST_DE_CFG = 0x10;
    static const u32 FAMILY_GRIFFIN = 0x11;
    static const u32 AMD_DE_CFG_MSR = 0xC0011029;
    static const u64 DE_CFG_LFENCE_SERIALIZE = 1ULL << 1;

    CpuidResult v = cpuid(LEAF_VENDOR);
    bool amd = v.ebx == AMD_EBX && v.edx == AMD_EDX && v.ecx == AMD_ECX;
    bool hygon = v.ebx == HYGON_EBX && v.edx == HYGON_EDX && v.ecx == HYGON_ECX;
    if (!amd && !hygon)
        return false;

    CpuidResult f = cpuid(LEAF_FEATURES);
    if (f.ecx & FEATURES_ECX_HYPERVISOR)
        return false;
    u32 family = (f.eax >> FAMILY_SHIFT) & FAMILY_MASK;
    if (family == FAMILY_EXTENDED)
        family += (f.eax >> EXT_FAMILY_SHIFT) & EXT_FAMILY_MASK;
    if (family < FAMILY_FIRST_DE_CFG || family == FAMILY_GRIFFIN)
        return false;

    u64 cfg = read_msr(AMD_DE_CFG_MSR);
    if (cfg & DE_CFG_LFENCE_SERIALIZE)
        return false;
    write_msr(AMD_DE_CFG_MSR, cfg | DE_CFG_LFENCE_SERIALIZE);
    return true;
}

bool setup_cpu_performance(CpuPerformance* out)
{
    static const u32 LEAF_BASIC = 0;
    static const u32 LEAF_FEATURES = 1;
    static const u32 LEAF_POWER = 6;
    static const u32 FEATURES_ECX_HYPERVISOR = 1U << 31;
    /* CPUID.06H:EAX: HWP, and its energy/performance preference field. */
    static const u32 POWER_EAX_HWP = 1U << 7;
    static const u32 POWER_EAX_HWP_EPP = 1U << 10;
    static const u32 PM_ENABLE_MSR = 0x770;
    static const u64 PM_ENABLE_HWP = 1ULL << 0;
    static const u32 HWP_CAPABILITIES_MSR = 0x771;
    static const u32 HWP_REQUEST_MSR = 0x774;
    /* Every field of both MSRs used here is a byte. */
    static const u64 FIELD_MASK = 0xFF;
    static const u32 CAP_HIGHEST_SHIFT = 0;
    static const u32 CAP_GUARANTEED_SHIFT = 8;
    static const u32 CAP_LOWEST_SHIFT = 24;
    static const u32 REQ_MIN_SHIFT = 0;
    static const u32 REQ_MAX_SHIFT = 8;
    static const u32 REQ_DESIRED_SHIFT = 16;
    static const u32 REQ_EPP_SHIFT = 24;
    /* intel_pstate's default preference, "balance_performance". */
    static const u64 EPP_BALANCE_PERFORMANCE = 0x80;

    *out = CpuPerformance{};
    if (kernel::Parameters::get_instance().is_hwp_off())
        return false;
    if (cpuid(LEAF_BASIC).eax < LEAF_POWER)
        return false;
    if (cpuid(LEAF_FEATURES).ecx & FEATURES_ECX_HYPERVISOR)
        return false;
    u32 power = cpuid(LEAF_POWER).eax;
    if (!(power & POWER_EAX_HWP))
        return false;

    /* HWP on first: every HWP MSR but IA32_PM_ENABLE and the interrupt
       one may be touched only once it is (SDM 15.4.2). A first version read
       the capabilities before, and the EX44 -- no serial port, no network up
       yet -- never answered from that boot and had to be reset by hand. */
    u64 enable = read_msr(PM_ENABLE_MSR);
    if (!(enable & PM_ENABLE_HWP))
        write_msr(PM_ENABLE_MSR, enable | PM_ENABLE_HWP);

    /* A range that makes no sense leaves HWP choosing within whatever
       request it starts with, rather than one built from it. */
    u64 caps = read_msr(HWP_CAPABILITIES_MSR);
    u64 highest = (caps >> CAP_HIGHEST_SHIFT) & FIELD_MASK;
    u64 guaranteed = (caps >> CAP_GUARANTEED_SHIFT) & FIELD_MASK;
    u64 lowest = (caps >> CAP_LOWEST_SHIFT) & FIELD_MASK;
    if (highest == 0 || lowest > highest || guaranteed > highest)
        return false;

    /* The whole range, and no desired level: the CPU picks its clock
       itself, inside it. What else the request holds -- an activity window,
       a package-level request -- stays as firmware left it. */
    u64 request = read_msr(HWP_REQUEST_MSR);
    request &= ~((FIELD_MASK << REQ_MIN_SHIFT) | (FIELD_MASK << REQ_MAX_SHIFT) | (FIELD_MASK << REQ_DESIRED_SHIFT));
    request |= (lowest << REQ_MIN_SHIFT) | (highest << REQ_MAX_SHIFT);
    if (power & POWER_EAX_HWP_EPP)
    {
        request &= ~(FIELD_MASK << REQ_EPP_SHIFT);
        request |= EPP_BALANCE_PERFORMANCE << REQ_EPP_SHIFT;
    }
    write_msr(HWP_REQUEST_MSR, request);

    out->lowest = (u32)lowest;
    out->guaranteed = (u32)guaranteed;
    out->highest = (u32)highest;
    return true;
}

ulong mmio_premapped_va(ulong phys_addr, ulong size_bytes)
{
    (void)phys_addr;
    (void)size_bytes;
    return 0;
}

void sync_instruction_cache(ulong va, ulong size)
{
    /* Instruction fetch snoops stores on x86, this CPU's and every other's:
       code written through a data mapping can be fetched as soon as its
       pages are mapped executable. Nothing to clean, nothing to invalidate. */
    (void)va;
    (void)size;
}

u16 module_elf_machine()
{
    return kernel::elf::MACHINE_X86_64;
}

ModuleReloc classify_module_reloc(u32 type)
{
    /* x86-64 psABI numbering */
    static const u32 REL_NONE = 0;
    static const u32 REL64 = 1;       /* R_X86_64_64: a pointer in data */
    static const u32 REL_GLOB_DAT = 6;  /* R_X86_64_GLOB_DAT: a GOT slot */
    static const u32 REL_JUMP_SLOT = 7; /* R_X86_64_JUMP_SLOT: a PLT's slot */
    static const u32 REL_RELATIVE = 8; /* R_X86_64_RELATIVE */

    switch (type)
    {
    case REL_NONE:
        return ModuleReloc::None;
    case REL_RELATIVE:
        return ModuleReloc::Relative;
    case REL64:
    case REL_GLOB_DAT:
    case REL_JUMP_SLOT:
        return ModuleReloc::Symbol;
    default:
        return ModuleReloc::Unsupported;
    }
}

/* The ABI's stack alignment: RSP on a 16-byte boundary at every call, so
   that a function is entered 8 below one. */
static const ulong ABI_STACK_ALIGN = 16;

ulong build_task_frame(ulong stack_top, ulong entry, ulong arg)
{
    /* The entry function is reached by switch_context's `ret`, as though it
       had been called from a boundary -- so the address `ret` pops sits on
       one. Worked out from the top rounded down, not from where Task::Stack
       happens to put stack_top. */
    ulong* rsp = (ulong *)((stack_top & ~(ABI_STACK_ALIGN - 1)) - ABI_STACK_ALIGN);
    rsp[0] = entry; /* return address switch_context's ret pops */
    /* and the entry function's own, which it has none of: 0 ends a
       backtrace there (StackTrace::capture_from), where the stack's poison
       read as one more frame of every task's */
    rsp[1] = 0;
    kernel::Context* regs = (kernel::Context*)((ulong)rsp - sizeof(*regs));
    stdlib::mem_set(regs, 0, sizeof(*regs));
    regs->rdi = arg;         /* 1st argument for the entry function */
    regs->rflags = (1 << 9); /* IF */
    return (ulong)regs;
}

namespace
{

void print_feature(stdlib::Printer& con, const char* name, bool present,
    const char* why)
{
    con.printf("  %s %s  %s\n", present ? "yes" : "NO ", name, why);
}

}

void print_cpu_info(stdlib::Printer& con)
{
    static const u32 EXTENDED_LEAF_BASE = 0x80000000;

    CpuidResult r0 = cpuid(0);

    /* Vendor string is EBX:EDX:ECX, in that order. */
    char vendor[13];
    stdlib::mem_cpy(&vendor[0], &r0.ebx, 4);
    stdlib::mem_cpy(&vendor[4], &r0.edx, 4);
    stdlib::mem_cpy(&vendor[8], &r0.ecx, 4);
    vendor[12] = '\0';

    u32 max_extended = cpuid(EXTENDED_LEAF_BASE).eax;

    char brand[49];
    brand[0] = '\0';
    if (max_extended >= EXTENDED_LEAF_BASE + 4)
    {
        for (u32 i = 0; i < 3; i++)
        {
            CpuidResult b = cpuid(EXTENDED_LEAF_BASE + 2 + i);
            stdlib::mem_cpy(&brand[i * 16 + 0], &b.eax, 4);
            stdlib::mem_cpy(&brand[i * 16 + 4], &b.ebx, 4);
            stdlib::mem_cpy(&brand[i * 16 + 8], &b.ecx, 4);
            stdlib::mem_cpy(&brand[i * 16 + 12], &b.edx, 4);
        }
        brand[48] = '\0';
    }

    CpuidResult r1 = cpuid(1);

    /* Family and model carry their extended halves above 0xF / 0x6
       respectively (SDM 3-217). */
    ulong family = (r1.eax >> 8) & 0xF;
    ulong model = (r1.eax >> 4) & 0xF;
    ulong stepping = r1.eax & 0xF;
    if (family == 0xF)
        family = family + ((r1.eax >> 20) & 0xFF);
    if (family == 0x6 || family == 0xF)
        model = model + (((r1.eax >> 16) & 0xF) << 4);

    con.printf("vendor %s\n", vendor);
    if (brand[0] != '\0')
        con.printf("model  %s\n", brand);
    con.printf("family %lu model %lu stepping %lu, max leaf 0x%lX ext 0x%lX\n",
        family, model, stepping, (ulong)r0.eax, (ulong)max_extended);

    CpuidResult ext1 = (max_extended >= EXTENDED_LEAF_BASE + 1)
        ? cpuid(EXTENDED_LEAF_BASE + 1) : CpuidResult{0, 0, 0, 0};
    CpuidResult ext7 = (max_extended >= EXTENDED_LEAF_BASE + 7)
        ? cpuid(EXTENDED_LEAF_BASE + 7) : CpuidResult{0, 0, 0, 0};
    CpuidResult r6 = (r0.eax >= 6) ? cpuid(6) : CpuidResult{0, 0, 0, 0};

    con.printf("features the kernel depends on:\n");
    print_feature(con, "pdpe1gb ", (ext1.edx & (1u << 26)) != 0,
        "1GiB pages: RAM above 4GiB");
    print_feature(con, "nx      ", (ext1.edx & (1u << 20)) != 0,
        "no-execute: W^X");
    print_feature(con, "pat     ", (r1.edx & (1u << 16)) != 0,
        "write-combining framebuffer");
    print_feature(con, "invtsc  ", (ext7.edx & (1u << 8)) != 0,
        "invariant tsc: timekeeping");
    print_feature(con, "arat    ", (r6.eax & (1u << 2)) != 0,
        "always-running apic timer: the per-cpu tick");
    print_feature(con, "x2apic  ", (r1.ecx & (1u << 21)) != 0,
        "firmware may hand off in this mode");
    print_feature(con, "tscdl   ", (r1.ecx & (1u << 24)) != 0,
        "tsc-deadline timer (unused)");
    print_feature(con, "cx16    ", (r1.ecx & (1u << 13)) != 0,
        "cmpxchg16b (unused)");
    print_feature(con, "hyperv  ", (r1.ecx & (1u << 31)) != 0,
        "running under a hypervisor");

    /* What the profiler can sample with. The two vendors enumerate entirely
       different things here, and CPUID.0AH on an AMD part reads back as
       zeroes -- printing it there would say "no counters" about a machine
       that has six. */
    static const u32 AMD_VENDOR_EBX = 0x68747541;   /* "Auth" */
    if (r0.ebx == AMD_VENDOR_EBX)
    {
        /* Fn8000_0022 is PerfMonV2: global control and status, Zen 4 and
           later, and the only place AMD states a counter count. Without it
           the count follows Fn8000_0001_ECX[PerfCtrExtCore]: six with, the
           four legacy ones without. Width is 48 bits on every AMD part and
           is enumerated nowhere. */
        CpuidResult r22 = (max_extended >= EXTENDED_LEAF_BASE + 0x22)
            ? cpuid(EXTENDED_LEAF_BASE + 0x22) : CpuidResult{0, 0, 0, 0};
        bool perf_mon_v2 = (r22.eax & 1u) != 0;
        bool ext_core = (ext1.ecx & (1u << 23)) != 0;

        con.printf("amd perfmon: %lu core counters 48 bits, perfctr-core %s, "
            "perfmon v2 %s\n",
            perf_mon_v2 ? (ulong)(r22.ebx & 0xF) : (ext_core ? 6UL : 4UL),
            ext_core ? "yes" : "NO", perf_mon_v2 ? "yes" : "no");
    }
    else
    {
        /* CPUID.0AH: architectural performance monitoring. Version 0 means
           the counters are not there to program -- which is the case under
           TCG, and is what decides whether this machine can sample faster
           than its tick. */
        CpuidResult r10 = (r0.eax >= 0xA) ? cpuid(0xA) : CpuidResult{0, 0, 0, 0};
        ulong perf_version = r10.eax & 0xFF;
        con.printf("arch perfmon: version %lu, %u general counters %u bits, "
            "%lu fixed counters %lu bits\n",
            perf_version, (r10.eax >> 8) & 0xFF, (r10.eax >> 16) & 0xFF,
            (ulong)(r10.edx & 0x1F), (ulong)((r10.edx >> 5) & 0xFF));
    }

    con.printf("profile sampling: %s\n",
        pmu_available() ? pmu_name() : "tick only");

    /* Only ever non-zero on AMD, and only after a profiling run. Printed
       here because the NMI that produced it could not safely say so
       itself. */
    ulong spurious = kernel::Pmu::spurious_nmi_count();
    if (spurious != 0)
        con.printf("  %lu late performance-counter nmis absorbed\n", spurious);
}

bool pmu_available()
{
    return kernel::Pmu::available();
}

bool pmu_start()
{
    return kernel::Pmu::start(kernel::Pmu::DEFAULT_PERIOD);
}

void pmu_stop()
{
    kernel::Pmu::stop();
}

const char* pmu_name()
{
    return kernel::Pmu::name();
}

void print_cpu_state(stdlib::Printer& con)
{
    con.printf("ss 0x%lX cs 0x%lX ds 0x%lX gs 0x%lX fs 0x%lX es 0x%lX",
        (ulong)get_ss(), (ulong)get_cs(), (ulong)get_ds(),
        (ulong)get_gs(), (ulong)get_fs(), (ulong)get_es());

    con.printf("rflags 0x%lX rsp 0x%lX rip 0x%lX\n",
        get_rflags(), get_rsp(), get_rip());

    con.printf("cr0 0x%lX cr2 0x%lX cr3 0x%lX cr4 0x%lX",
        get_cr0(), get_cr2(), get_cr3(), get_cr4());
}

ulong task_saved_frame_pointer(ulong saved_sp, ulong stack_base, ulong stack_limit)
{
    /* switch_context's PushAll is a Context, all of it */
    if (saved_sp % sizeof(ulong) != 0 || saved_sp < stack_base || saved_sp > stack_limit ||
        stack_limit - saved_sp < sizeof(kernel::Context))
        return 0;

    return ((kernel::Context*)saved_sp)->rbp;
}

void run_on_stack(ulong stack_top, void (*fn)(void*), void* ctx)
{
    /* Rounded down before the call, as arm64_run_on_stack does. Task::Stack's
       stack_top sits 8 below a boundary, and a call from there entered every
       function on the boot and idle stacks 8 off the ABI -- which nothing
       noticed, the kernel using no SSE, until a UBSan build (docs/build.md)
       found a 16-aligned local 8 off. */
    ulong top = stack_top & ~(ABI_STACK_ALIGN - 1);
    asm volatile(
        "movq %1, %%rsp\n\t"
        "callq *%0\n\t"
        :: "a"(fn), "r"(top), "D"(ctx)
        : "memory");
    trace(0, "run_on_stack: fn returned");
    while (1)
        cpu_halt();
}

namespace
{

bool use_vga()
{
    return !kernel::Parameters::get_instance().is_console_serial();
}

bool use_serial()
{
    return !kernel::Parameters::get_instance().is_console_vga();
}

}

void console_out(const char *s)
{
    if (use_vga())
        kernel::screen::print_string(s);
    if (use_serial())
        kernel::Serial::get_instance().print_string(s);
}

void console_out_backspace()
{
    if (use_vga())
        kernel::screen::backspace();
    if (use_serial())
        kernel::Serial::get_instance().backspace();
}

void console_out_clear()
{
    if (use_vga())
        kernel::screen::cls();
    if (use_serial())
    {
        /* ANSI escape: clear screen and move cursor home */
        kernel::Serial::get_instance().print_string("\033[2J\033[H");
    }
}

void console_write(const char *msg)
{
    kernel::Serial::get_instance().print_string(msg);
    kernel::Serial::get_instance().flush();

    /* The boot trace goes out the serial port and is mirrored on the screen
       only on request -- redrawing glyphs for every line costs a full
       repaint per scrolled row. A machine with no UART has nowhere else to
       put it, so there the mirror is the default rather than the option. */
    if ((kernel::Parameters::get_instance().is_trace_vga() ||
         !kernel::Serial::get_instance().is_present()) &&
        kernel::screen::is_ready())
    {
        kernel::screen::print_string(msg);
    }
}

void console_panic_write(const char *msg)
{
    kernel::Serial::get_instance().panic_print_string(msg);
    if (kernel::screen::is_ready())
        kernel::screen::panic_print_string(msg);
}

void power_off()
{
    trace(0, "ACPI shutdown");

    /* Try PM1a_CNT from FADT with SLP_TYP=5 (S5) | SLP_EN */
    ulong pm1a = kernel::Acpi::get_instance().get_pm1a_cnt_port();
    if (pm1a != 0)
    {
        outw((u16)pm1a, (5 << 10) | (1 << 13));
        /* Brief busy-wait for the hardware to respond */
        for (volatile int i = 0; i < 1000000; i = i + 1) {}
    }

    /* QEMU/KVM fallback: PM1a_CNT port 0x604, SLP_TYP=0 for S5 */
    outw(0x604, (1 << 13));

    /* QEMU debug exit device fallback */
    outb(0xf4, 0x0);

    while (1) cpu_halt();
}

void reset()
{
    trace(0, "Reboot");
    reset_now();
}

void reset_now()
{
    static const int SETTLE_SPINS = 1000000;

    /* Try ACPI FADT RESET_REG first: parsed or not, it is only read here */
    auto& acpi = kernel::Acpi::get_instance();
    if (acpi.has_reset_reg())
    {
        outb((u16)acpi.get_reset_reg_port(), acpi.get_reset_value());
        for (volatile int i = 0; i < SETTLE_SPINS; i = i + 1) {}
    }

    /* Keyboard controller reset (pulse CPU reset line) */
    outb(0x64, 0xFE);

    /* Fallback: PCI reset register */
    outb(0xCF9, 0x06);
    for (volatile int i = 0; i < SETTLE_SPINS; i = i + 1) {}

    /* Last: a triple fault -- an IDT with no entries, and an exception it
       cannot deliver -- which every x86 machine answers with a reset. */
    struct __attribute__((packed))
    {
        u16 limit;
        u64 base;
    } no_idt = { 0, 0 };
    asm volatile("lidt %0; int3" : : "m"(no_idt) : "memory");

    while (1) cpu_halt();
}

}
