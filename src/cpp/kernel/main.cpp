#include "trace.h"
#include "ubsan.h"
#include "panic.h"
#include "debug.h"
#include "atomic.h"
#include <arch/x86_64/gdt.h>
#include <arch/x86_64/idt.h>
#include "test.h"
#include <arch/x86_64/exception.h>
#include <arch/x86_64/asm.h>
#include <hal/power.h>
#include <hal/mmu.h>
#include <hal/random.h>
#include "cpu.h"
#include "cmd.h"
#include "interrupt.h"
#include "icxxabi.h"
#include "preempt.h"
#include "dmesg.h"
#include "watchdog.h"
#include "parameters.h"
#include "console.h"
#include "input.h"
#include "softirq.h"
#include "irq_balance.h"
#include "time.h"
#include <arch/x86_64/tsc.h>

#include <arch/x86_64/grub.h>
#include <arch/x86_64/microcode.h>

#include <lib/error.h>
#include <lib/stdlib.h>

#include <mm/new.h>
#include <mm/page_allocator.h>
#include <mm/memory_map.h>
#include <mm/allocator.h>
#include <mm/page_table.h>

#include <drivers/8042.h>
#include <drivers/screen.h>
#include <drivers/serial.h>
#include <arch/x86_64/pic.h>
#include <drivers/pit.h>
#include <drivers/hpet.h>
#include <drivers/acpi.h>
#include <arch/x86_64/lapic.h>
#include <arch/x86_64/ioapic.h>
#include <drivers/pci.h>

#include <kernel/stack_probe.h>
#include <kernel/module.h>
#include <arch/x86_64/percpu.h>

using namespace kernel;
using namespace stdlib;
using namespace consts;

const size_t CPU_STACK_SIZE = 8 * consts::PAGE_SIZE;
static char stack[MAX_CPUS][8 * consts::PAGE_SIZE] __attribute__((aligned(consts::PAGE_SIZE)));
static long stack_index;
static volatile long ap_started_flag;

#define ALLOC_CPU_STACK()                               \
do {                                                    \
    auto index = atomic_read_and_inc(&stack_index);         \
    if (index >= (long)MAX_CPUS)                         \
        PANIC("Can't allocate stack for cpu");          \
    set_rsp((long)&stack[index][CPU_STACK_SIZE-128]);      \
} while (false)

void trace_cpu_state(ulong cpu)
{
    trace(0, "Cpu %lu cr0 0x%lX cr2 0x%lX cr3 0x%lX cr4 0x%lX",
        cpu, get_cr0(), get_cr2(), get_cr3(), get_cr4());

    trace(0, "Cpu %lu rflags 0x%lX rsp 0x%lX rip 0x%lX",
        cpu, get_rflags(), get_rsp(), get_rip());

    trace(0, "Cpu %lu ss 0x%lX cs 0x%lX ds 0x%lX gs 0x%lX fs 0x%lX es 0x%lX",
        cpu, (ulong)get_ss(), (ulong)get_cs(), (ulong)get_ds(),
        (ulong)get_gs(), (ulong)get_fs(), (ulong)get_es());
}

/*
 * Halt the Intel TCO (Total Cost of Ownership) hardware watchdog.
 *
 * The BIOS may leave the TCO timer running with a short timeout.  If
 * nothing kicks it before the second expiry the PCH resets the system.
 * We halt it here so the kernel can run self-tests in peace; the Rust
 * tco_wdt driver re-enables it later with a proper timeout.
 *
 * Discovery mirrors the Rust TcoWatchdog::probe() (keep the two in sync):
 *   1. 100-series PCH ("Sunrise Point") and later: TCOBASE from the SMBus
 *      function 0:1F.4 config reg 0x50, valid only while TCO_BASE_EN
 *      (TCOCTL, reg 0x54 bit 8) is set.  The LPC registers below read back
 *      as zero on these chipsets, so this comes first.
 *   2. Read TCOBASE from LPC bridge PCI config reg 0x50.
 *   3. Fallback: ACPI PMBASE (reg 0x40) + 0x60.
 *   4. Set TMR_HLT (bit 11) in TCO1_CNT to stop the timer.
 *
 * This runs before the PCI subsystem exists, hence the raw CF8/CFC access,
 * but after Acpi::Parse(), so the WDAT verdict is already available.
 */
static void halt_tco_watchdog()
{
    static const u16 PCI_CFG_ADDR  = 0xCF8;
    static const u16 PCI_CFG_DATA  = 0xCFC;
    static const u32 LPC_BDF      = (0U << 16) | (0x1FU << 11) | (0U << 8);
    static const u32 SMBUS_BDF    = (0U << 16) | (0x1FU << 11) | (4U << 8);
    static const u16 VENDOR_REG   = 0x00;
    static const u16 CLASS_REG    = 0x08;
    static const u16 TCO_BASE_REG  = 0x50;
    static const u16 TCO_CTL_REG   = 0x54;
    static const u32 TCO_BASE_EN   = 1U << 8;
    static const u8  SERIAL_BUS_CLS = 0x0C;
    static const u8  SMBUS_SUB_CLS  = 0x05;
    static const u16 ACPI_BASE_REG = 0x40;
    static const u16 ACPI_TCO_OFF  = 0x60;
    static const u16 TCO1_STS     = 0x04;
    static const u16 TCO2_STS     = 0x06;
    static const u16 TCO1_CNT     = 0x08;
    static const u16 TMR_HLT      = 1 << 11;
    static const u16 INTEL_VENDOR = 0x8086;

    trace(0, "HaltTcoWatchdog: started");

    /* An ACPI WDAT table means the firmware owns the watchdog and the Rust
       tco_wdt driver stands down.  The halt below still runs: WDAT normally
       describes this very TCO block, and a timer the firmware left running
       would reset the box in the middle of the boot-time self-tests with
       nothing left to kick it. */
    const bool firmware_watchdog = Acpi::get_instance().has_firmware_watchdog();

    u16 tco_base = 0;
    const char* source = "SMBus 0:1F.4";

    /* 1. 100-series PCH and later.  0:1F.4 is not the SMBus controller on
       every chipset, so check the class code before trusting its 0x50. */
    outl(PCI_CFG_ADDR, 0x80000000 | SMBUS_BDF | VENDOR_REG);
    if ((u16)inl(PCI_CFG_DATA) == INTEL_VENDOR)
    {
        outl(PCI_CFG_ADDR, 0x80000000 | SMBUS_BDF | CLASS_REG);
        u32 class_code = inl(PCI_CFG_DATA);
        if ((u8)(class_code >> 24) == SERIAL_BUS_CLS && (u8)(class_code >> 16) == SMBUS_SUB_CLS)
        {
            /* Without TCO_BASE_EN the window is not decoded */
            outl(PCI_CFG_ADDR, 0x80000000 | SMBUS_BDF | TCO_CTL_REG);
            if ((inl(PCI_CFG_DATA) & TCO_BASE_EN) != 0)
            {
                outl(PCI_CFG_ADDR, 0x80000000 | SMBUS_BDF | TCO_BASE_REG);
                tco_base = (u16)inl(PCI_CFG_DATA) & 0xFFE0;
            }
        }
    }

    /* 2./3. ICH9-class chipsets, including QEMU's q35 */
    if (tco_base == 0)
    {
        source = "LPC 0:1F.0";

        /* Only Intel PCH has a TCO watchdog */
        outl(PCI_CFG_ADDR, 0x80000000 | LPC_BDF | VENDOR_REG);
        u16 vendor = (u16)inl(PCI_CFG_DATA);
        if (vendor != INTEL_VENDOR)
            return;

        outl(PCI_CFG_ADDR, 0x80000000 | LPC_BDF | TCO_BASE_REG);
        u16 raw = (u16)inl(PCI_CFG_DATA);
        if (raw != 0 && raw != 0xFFFF)
            tco_base = raw & 0xFFE0;

        if (tco_base == 0) {
            outl(PCI_CFG_ADDR, 0x80000000 | LPC_BDF | ACPI_BASE_REG);
            u16 pm_base = (u16)inl(PCI_CFG_DATA) & 0xFFFE;
            if (pm_base != 0 && pm_base != 0xFFFE)
                tco_base = pm_base + ACPI_TCO_OFF;
        }
    }

    if (tco_base == 0)
        return;

    /* Halt the timer */
    u16 cnt = inw(tco_base + TCO1_CNT);
    outw(tco_base + TCO1_CNT, cnt | TMR_HLT);

    /* Clear stale timeout status so the first-expiry latch is reset */
    outw(tco_base + TCO1_STS, 1 << 3);
    outw(tco_base + TCO2_STS, 1 << 1);

    trace(0, "TCO watchdog halted (tcoBase 0x%lX via %s%s)", (ulong)tco_base, source,
        firmware_watchdog ? ", ACPI WDAT owns it" : "");
}

/* Calibrated once on the BSP; every core's local timer runs off the same bus
   clock, so one measurement arms all of them. False means calibration failed
   and the old broadcast tick stays in charge. */
static bool lapic_timer_ok;

void ap_startup(void *ctx)
{
    (void)ctx;

    auto& cpu = CpuTable::get_instance().get_current_cpu();

    trace(0, "Cpu %lu running rflags 0x%lX task 0x%p",
        cpu.get_index(), get_rflags(), Task::get_current_task());

    trace_cpu_state(cpu.get_index());

    /* Before interrupts: this CPU's own tick, so it never depends on another
       CPU still answering one -- one-shot, armed for each tick and for each
       deadline a task here sleeps to (Cpu::LocalTimerStart). */
    if (lapic_timer_ok)
    {
        Lapic::start_timer(CpuTable::TIMER_VECTOR);
        cpu.local_timer_start();
    }

    BUG_ON(hal::is_interrupt_enabled());
    interrupt_enable();

    cpu.set_running();

    preempt_on_wait();

    if (!test::test_multi_tasking())
    {
        PANIC("Multitasking test failed");
        return;
    }

    for (;;)
    {
        cpu.idle();
    }
}

/* Never inlined: called just after ALLOC_CPU_STACK has moved the stack
   pointer, and the caller's frame is still addressed off the frame pointer
   of the stack it left. Inlined, this function's locals would live in that
   frame -- on the trampoline's stack, which the real page tables do not
   map. A call starts a frame of its own on the new stack. */
__attribute__((noinline)) void ap_main2()
{
    atomic_read_and_inc(&ap_started_flag); /* 2: entered ApMain2 */

    /* Until the CR3 below, this CPU is still on the boot page table from
       boot64.asm, which maps exactly two windows: the low 4GiB identity and
       the same 4GiB at KernelSpaceBase. Nothing here may touch a virtual
       address outside them -- and the console is one. MapMmioRegion places a
       device at physAddr + KernelSpaceBase, so a framebuffer above 4GiB (the
       Raptor Lake iGPU aperture sits at 0x40_0000_0000) lands at a VA the
       boot table has no L3 entry for. An AP has no IDT yet either, so that
       page fault is a triple fault: the machine resets during StartAll with
       nothing on the screen and nothing in any log.

       Hence the order: NXE first (the W^X page table sets NX bits, which
       fault as reserved without it), then the real page table, then the GDT
       and IDT so a fault from here on is a panic report rather than a reset,
       and only then anything that can print. */
    hal::enable_wx_support();
    set_cr3(mm::PageTable::get_instance().get_root());
    atomic_read_and_inc(&ap_started_flag); /* 3: page tables loaded */

    Gdt::get_instance().save();
    Idt::get_instance().save();
    atomic_read_and_inc(&ap_started_flag); /* 4: GDT+IDT loaded */

    /* The update the BSP kept, as early as a fault here is a report: said
       below, once this CPU's index is known */
    microcode::Result ucode;
    microcode::load_on_ap(&ucode);

    /* Same PAT as the BSP before this CPU can touch a write-combining
       mapping (the framebuffer console is already mapped by now). Its trace
       is the first console write an AP makes, which is why it has to come
       after the CR3 load; the only mapping the two disagree about in between
       is the framebuffer, and nothing above touches it. */
    hal::setup_memory_types();
    /* As the BSP did: every CPU the same, or an lfence; rdtsc on this one
       would not be ordered as it is on the others. */
    hal::setup_serializing_lfence();

    if (Parameters::get_instance().is_smp_off())
        PANIC("AP cpu started while smp is off");

    Lapic::enable();
    atomic_read_and_inc(&ap_started_flag); /* 5: LAPIC enabled */

    /* KVM updates a pvclock entry only for the vcpu that armed it; without
       this the AP would read the BSP's entry with its own TSC. No-op when
       kvmclock is unavailable. */
    Tsc::get_instance().enable_kvm_clock_self();

    auto& cpu = CpuTable::get_instance().get_current_cpu();

    if (microcode::kept())
    {
        if (ucode.skipped == nullptr)
            trace(0, "Cpu %lu: microcode revision 0x%x -> 0x%x", cpu.get_index(), ucode.before, ucode.after);
        else if (ucode.asked)
            trace(0, "Cpu %lu: microcode %s (revision 0x%x)", cpu.get_index(), ucode.skipped, ucode.before);
        else
            trace(0, "Cpu %lu: microcode %s", cpu.get_index(), ucode.skipped);
    }

    /* Its clock its own to raise, as the BSP's is: each CPU's range its own,
       the two kinds of core of a hybrid part giving different ones. */
    hal::CpuPerformance perf;
    if (hal::setup_cpu_performance(&perf))
        trace(0, "Cpu %lu: HWP on, performance %u to %u, %u guaranteed",
            cpu.get_index(), perf.lowest, perf.highest, perf.guaranteed);

    /* GDT is loaded (above); give this CPU its TSS so #DF has an IST stack */
    Gdt::get_instance().setup_tss_self(cpu.get_index());

    trace(0, "Cpu %lu rsp 0x%lX (static stack)", cpu.get_index(), get_rsp());

    if (!cpu.run(ap_startup, nullptr))
    {
        trace(0, "Can't run cpu %lu task", cpu.get_index());
        return;
    }
}

extern "C" void ap_main()
{
    /* Before anything can ask which CPU this is: the GS base firmware left
       behind is not a slot of ours, and reading through it would be a wild
       load. Until Lapic::Enable() publishes the real one, this reads as
       "not set up" and the answer comes from the APIC as it always did. */
    per_cpu_setup_boot();

    atomic_read_and_inc(&ap_started_flag);
    ALLOC_CPU_STACK();
    ap_main2();
}

typedef void (*HaltAction)();

extern "C" void rust_fini();

/* Runs entirely on the static stack with its own stack frame.
   Releases the idle task, runs static destructors, then
   performs the final halt/reboot action. Never returns. */
static void __attribute__((noinline, noreturn))
finalize_on_static_stack(Task* task, HaltAction action)
{
    trace(0, "FinalizeOnStaticStack");

    task->put();

    rust_fini();

    __cxa_finalize(0);

    trace(0, "Finalized");

    action();

    while (1) cpu_halt();
}

static void prepare_halt(HaltAction action)
{
    /* Hold an extra reference so CpuTable::Reset()'s Put()
    only drops refcount 2→1 instead of freeing the task
    while we are still running on its stack. */
    auto task = Task::get_current_task();
    task->get();

    preempt_disable();

    trace(0, "Stopping cpu's");

    CpuTable::get_instance().exit_all_except_self();

    trace(0, "Cpu's stopped");

    preempt_off();

    CpuTable::get_instance().reset();
    Dmesg::get_instance().reset();

    interrupt_disable();

    /* Switch back to CPU 0's static stack so we can release
       the idle task.  FinalizeOnStaticStack gets its own
       stack frame on the static stack, calls task->Put()
       (refcount 1→0, frees the idle task and its dynamic
       stack), runs static destructors, and then performs the
       halt action.  It never returns, so PrepareHalt's stale
       RBP is harmless.

       WARNING: SetRsp abandons the caller's stack without
       unwinding.  Any C++ objects still alive on that stack
       (the idle task's stack) will NOT have their destructors
       called.  Callers of Shutdown()/Reboot() must ensure all
       stack-local objects with non-trivial destructors (e.g.
       SpinLock, or anything containing one) are destroyed
       before reaching this point. */
    set_rsp((long)&stack[0][CPU_STACK_SIZE - 128]);
    finalize_on_static_stack(task, action);
}

void shutdown()
{
    auto& con = Console::get_instance();
    con.printf("Shutting down!\n");

    prepare_halt(hal::power_off);
}

void reboot()
{
    auto& con = Console::get_instance();
    con.printf("Rebooting!\n");

    prepare_halt(hal::reset);
}

void some_task_routine(void *ctx)
{
    (void)ctx;

    auto task = Task::get_current_task();
    while (!task->is_stopping())
        sleep(100 * consts::NANO_SECS_IN_MS);
}

/* Shutdown()/Reboot() end up in PrepareHalt which abandons this
   stack via SetRsp without unwinding (see the WARNING there).
   All C++ objects with non-trivial destructors must go out of
   scope before those calls, so the body is wrapped in a block. */
extern "C" void rust_init();
/* The network layer is Rust (src/rust/net): the recycled frame pool, the
   log over UDP armed as soon as the command line is known, TCP, and the
   services boot starts once there is a device and a shell. */
extern "C" int rust_netframe_pool_setup(unsigned long count);
extern "C" int rust_netconsole_setup();
extern "C" int rust_tcp_init();
extern "C" void rust_net_start_services(unsigned short udp_shell_port);
extern "C" void rust_net_stop_services();
/* USB is Rust (src/rust/drivers/usb): the xHCI controllers and the HID boot
   keyboard on them, which on a machine with no PS/2 is the only way in. */
extern "C" void rust_usb_init();
extern "C" int rust_usb_start();
extern "C" void rust_usb_stop();

/* The block layer is Rust (src/rust/block): before this, a synchronous I/O
   has to poll its device, because there is nothing yet to wake a waiter. */
extern "C" void kernel_blockdev_set_interrupts_started();

/* The filesystem layer is Rust (src/rust/fs): what boot mounts where, and
   taking it all down again on the way out. */
extern "C" void rust_mount_root_fs();
extern "C" void kernel_vfs_unmount_all();

/* The disk log is Rust (src/rust/block/src/disklog.rs). */
extern "C" int rust_disklog_setup();
extern "C" void rust_disklog_stop();
extern "C" void rust_test();

/* The random pool is Rust (src/rust/random): seeded at boot from what the
   machine itself can be measured for, before anything asks it for bytes,
   and again once the devices that can do better are up. */
extern "C" bool rust_random_setup();
extern "C" void rust_random_reseed();
/* The partition table reader (src/rust/block): registers a block device for
   every partition of every disk the kernel has not looked at yet. */
extern "C" void rust_partitions_probe();
/* The virtio-blk driver (src/rust/drivers/virtio_blk): the disks come up
   here, before the partitions on them are looked at. */
extern "C" void rust_virtio_blk_init();
/* The virtio-net driver (src/rust/drivers/virtio_net) */
extern "C" void rust_virtio_net_init();
/* The virtio-scsi driver (src/rust/drivers/virtio_scsi) */
extern "C" void rust_virtio_scsi_init();

void bp_startup(void* ctx)
{
    (void)ctx;

    bool do_reboot = false;

    {
        auto& idt = Idt::get_instance();
        auto& pit = Pit::get_instance();
        auto& kbd = IO8042::get_instance();
        auto& serial = Serial::get_instance();
        auto& cmd = Cmd::get_instance();
        auto& io_apic = IoApic::get_instance();
        auto& cpus = CpuTable::get_instance();
        auto& cpu = cpus.get_current_cpu();
        auto& acpi = Acpi::get_instance();

        trace(0, "Cpu %lu running rflags 0x%lX task 0x%p",
            cpu.get_index(), get_rflags(), Task::get_current_task());

        trace_cpu_state(cpu.get_index());

        /* GDT is loaded (Main2); give the BSP its TSS so #DF has an IST
           stack */
        Gdt::get_instance().setup_tss_self(cpu.get_index());

        io_apic.enable();

        /* Choose tick source: HPET if available, otherwise PIT (ISA IRQ 0). */
        auto& hpet = Hpet::get_instance();
        if (hpet.is_available())
        {
            /* HPET in legacy replacement mode: timer 0 replaces PIT on IRQ 0.
               Use the same GSI that PIT would use (MADT may remap IRQ 0 → GSI 2). */
            Interrupt::register_handler(hpet, acpi.get_gsi_by_irq(0x0), 0x20);
            trace(0, "Using HPET as tick source (legacy replacement, IRQ 0)");
        }
        else
        {
            // PIT is ISA IRQ 0; ACPI MADT may remap it to GSI 2
            Interrupt::register_handler(pit, acpi.get_gsi_by_irq(0x0), 0x20);
            pit.setup();
            trace(0, "Using PIT as tick source");
        }

        Interrupt::register_handler(kbd, acpi.get_gsi_by_irq(0x1), 0x21);
        Interrupt::register_handler(serial, acpi.get_gsi_by_irq(0x4), 0x24);

        rust_virtio_blk_init();
        rust_virtio_scsi_init();
        rust_partitions_probe();

        /* Before any driver can want a frame: the pool is what keeps the
           datapath clear of the allocator, and of the TLB shootdown that
           freeing a frame would otherwise cost. That means before rust_init,
           whose NIC drivers fill their receive rings as they come up. Set up
           after it, as it was, a ring started out as a thousand frames from
           Mm::Alloc, and the first burst after boot paid a shootdown for each
           in the receive softirq until the ring had turned over: on the AX41
           two seconds at 256 frames a second, the NIC dropping the rest.
           arm64 has always had this order. */
        /* 0 means the layer's own default. */
        rust_netframe_pool_setup(Parameters::get_instance().get_net_frame_count());

        rust_init();

        rust_virtio_net_init();

        /* Now that the devices are here, fold what they can give into the
           pool: on a machine with a virtio-rng this is where it stops resting
           on what the boot itself could be measured for. */
        rust_random_reseed();

        trace(0, "Interrupts registered");

        idt.set_descriptor(CpuTable::IPI_VECTOR, IdtDescriptor::encode(ip_interrupt_stub));
        idt.set_descriptor(CpuTable::TIMER_VECTOR,
            IdtDescriptor::encode(lapic_timer_interrupt_stub));
        idt.set_descriptor(hal::POSTED_VECTOR, IdtDescriptor::encode(posted_interrupt_stub));

        /* Install a benign handler for the LAPIC spurious-interrupt vector so a
           spurious IRQ counts a stat instead of hitting DummyInterrupt's panic. */
        idt.set_descriptor(Lapic::SPURIOUS_VECTOR, IdtDescriptor::encode(spurious_interrupt_stub));

        trace(0, "IPI registred");

        idt.save();

        trace(0, "Idt saved");

        trace(0, "Interrupts enabled %lu", (ulong)hal::is_interrupt_enabled());

        BUG_ON(hal::is_interrupt_enabled());

        trace(0, "Before interrupt enable");

        interrupt_enable();

        kernel_blockdev_set_interrupts_started();

        trace(0, "Interrupts enabled %lu", (ulong)hal::is_interrupt_enabled());

        /*
         * TSC calibration uses PIT channel 2 with busy-polling (~150ms).
         * Done after InterruptEnable so PIT channel 0 interrupts flow
         * normally, and before cpus.StartAll so the clock source is ready
         * for per-CPU time queries.
         */
        time_init();

        /* One local APIC timer per CPU in place of one HPET interrupt fanned
           out to everybody by IPI. The fan-out cost a write to the interrupt
           command register per CPU per tick, but the real problem was that it
           made every CPU's sense of time depend on one CPU still answering
           interrupts: when that one wedged, nothing anywhere ticked, so
           nothing preempted and no watchdog ran to notice. Now a wedged CPU
           stops only its own tick and the others go on watching it.

           Calibration needs a clock, which is why this sits after TimeInit,
           and it must precede StartAll so an AP can arm its timer as it comes
           up. arm64 has worked this way from the start (GenericTimer). */
        lapic_timer_ok = Lapic::calibrate_timer();
        if (lapic_timer_ok)
        {
            Lapic::start_timer(CpuTable::TIMER_VECTOR);
            cpus.get_current_cpu().local_timer_start();
            cpus.set_per_cpu_timer();
            trace(0, "Tick: per-cpu local apic timer, one-shot, at %lu Hz", TICK_HZ);
        }
        else
        {
            trace(0, "Tick: local apic timer unavailable, keeping the broadcast tick");
        }

        trace(0, "Before cpus start");

        if (!Parameters::get_instance().is_smp_off())
        {
            if (!cpus.start_all())
            {
                /* Diagnostic: did the AP reach ApMain()? */
                trace(0, "AP diag: ApStartedFlag=%lu",
                      (ulong)ap_started_flag);
                PANIC("Can't start all cpus");
                return;
            }
        }

        /* All CPUs are running: spread device IRQs across them */
        IrqBalance::get_instance().balance();

        trace(0, "Before preempt on");

        preempt_on();

        trace(0, "Preempt is now on");

        screen::printf("IPI test...\n");

        ulong cpu_mask = cpus.get_running_cpus();
        for (ulong i = 0; i < MAX_CPUS; i++)
        {
            if (cpu_mask & (1UL << i))
            {
                if (i != cpu.get_index())
                {
                    cpus.send_ipi(i);
                }
            }
        }

        screen::printf("Task test...\n");

        if (!test::test_multi_tasking())
        {
            PANIC("Multitasking test failed");
            return;
        }

        if (!test::test_task_locate())
        {
            PANIC("Task locate test failed");
            return;
        }

        if (!test::test_wake_storm())
        {
            PANIC("Wake storm test failed");
            return;
        }

        if (!test::test_idle_takes_turns())
        {
            PANIC("Idle task turn test failed");
            return;
        }

        if (!test::test_eevdf())
        {
            PANIC("EEVDF test failed");
            return;
        }

        rust_test();

        if (!SoftIrq::get_instance().init())
        {
            PANIC("Can't init softirq");
            return;
        }

        /* The disks that came up in rust_init -- NVMe, above all -- were
           registered after the boot-time probe ran, so their partitions have
           never been looked at. Done here rather than next to that probe for
           the same reason the log area is: until the soft IRQ layer exists a
           block read returns without having read anything, so a probe there
           would find no partitions and cache that as the answer. */
        rust_partitions_probe();

        /* The root filesystem may well be on one of those disks */
        rust_mount_root_fs();

        /* Here, and not earlier: a block request is completed through the
           BLK_IO soft IRQ, so until the line above a read returns without
           having read anything. Everything traced since the first line of the
           boot has been held in memory and goes down as soon as the area is
           found, so the log on disk is whole from the beginning -- but a
           machine that stops before this point leaves nothing, because
           nothing can be written yet. Closing that gap means a polled
           completion path in the NVMe driver. */
        rust_disklog_setup();

        rust_tcp_init();

        /* On a machine with no PS/2 controller the xHCI HID keyboard is the
           only way in, so bring it up before the shell starts: enumeration
           happens here, synchronously, while trace output still reaches the
           console, and the polling task takes over afterwards. */
        if (!Parameters::get_instance().is_usb_off())
        {
            rust_usb_init();

            if (rust_usb_start() != 0)
                trace(0, "Usb: failed to start the poll task");
        }

        screen::printf("Idle looping...\n");

        if (!cmd.start())
        {
            PANIC("Can't start cmd");
            return;
        }

        /* The netconsole, if the command line asked for one, and the shell over
           UDP: both on eth0, both the layer's to start. */
        rust_net_start_services(Parameters::get_instance().get_udp_shell_port());

        trace(0, "boot: complete");

        for (;;)
        {
            cpu.idle();
            if (cmd.should_shutdown() || cmd.should_reboot())
            {
                do_reboot = cmd.should_reboot();
                if (do_reboot)
                    trace(0, "Reboot requested");
                else
                    trace(0, "Shutdown requested");
                rust_net_stop_services();
                rust_usb_stop();
                cmd.stop();
                cmd.stop_dhcp();
                break;
            }
        }

        /* With the shells gone nothing calls into a module any more: its
           exit runs now, while the files, block I/O and soft IRQs it may
           still need are all there */
        ModuleTable::get_instance().unload_all();

        /* Before the soft IRQs stop: unmounting writes the superblock, and
           a block request completes through the BLK_IO soft IRQ */
        kernel_vfs_unmount_all();

        /* The disk log last of all: its writer finishes what is queued and
           the log switches off -- after SoftIrq::Stop() a write through a
           virtio disk would wait for ever. */
        rust_disklog_stop();

        SoftIrq::get_instance().stop();
    } /* all locals destroyed before stack is abandoned */

    if (do_reboot)
        reboot();
    else
        shutdown();
}

/* wxprobe=heap: call into a page the allocator just handed out. Every
   runtime mapping is NX, so the call must die on the instruction fetch; the
   line after it printing means W^X covers the kernel image and nothing else.
   Deliberately fatal, like the text probe -- it is asked for by name on the
   command line. */
static void wx_probe_heap()
{
    static const u8 RET_OPCODE = 0xC3;

    ulong phys = 0;
    void* va = mm::alloc_map_pages(1, &phys);
    if (va == nullptr)
    {
        trace(0, "W^X probe: no page for the heap probe");
        return;
    }

    *reinterpret_cast<volatile u8*>(va) = RET_OPCODE;
    hal::sync_instruction_cache((ulong)va, sizeof(RET_OPCODE));
    trace(0, "W^X probe: calling into a heap page 0x%lX (expect page fault)", (ulong)va);
    reinterpret_cast<void (*)()>(va)();
    trace(0, "W^X probe: heap execute SUCCEEDED (W^X broken!)");
}

/* Never inlined, as ApMain2 is not: see there */
__attribute__((noinline)) void main2(grub::MultiBootInfoHeader *mb_info)
{
    do {

    Panicker::get_instance();
    Watchdog::get_instance();

    trace(0, "Cpu rsp 0x%lX rbp 0x%lX", get_rsp(), get_rbp());
    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        trace(0, "Static stack[%lu] base 0x%lX top 0x%lX",
            i, (ulong)&stack[i][0], (ulong)&stack[i][CPU_STACK_SIZE]);
    }

    /* Fill the per-CPU static stacks with the pattern whose survival tells us
       later how deep they went. Only the BSP is running here, so every other
       stack can be filled whole; the one this code is standing on is filled
       from its base to a margin below the stack pointer, since the fill runs
       on it. */
    {
        const ulong sp = get_rsp();
        const ulong poison_margin = 2048;

        for (ulong i = 0; i < MAX_CPUS; i++)
        {
            ulong base = (ulong)&stack[i][0];
            ulong size = CPU_STACK_SIZE;

            if (sp > base && sp < base + CPU_STACK_SIZE)
            {
                if (sp - base <= poison_margin)
                    continue;

                size = (sp - base - poison_margin) & ~(sizeof(ulong) - 1);
            }

            stack_probe::poison((void*)base, size);
        }
    }

    /* Before any page table is built: every mapping the kernel makes at
       runtime carries NX, and with EFER.NXE off bit 63 is a reserved bit --
       the first touch of such a page would fault instead of reading. The APs
       do the same before they load this table (ApMain2). */
    hal::enable_wx_support();

    auto& bpt = mm::BuiltinPageTable::get_instance();
    if (!bpt.setup())
    {
        PANIC("Can't setup paging");
        break;
    }

    trace(0, "Paging root 0x%lX old cr3 0x%lX", bpt.get_root(), get_cr3());
    set_cr3(bpt.get_root());
    trace(0, "Set new cr3 0x%lX", get_cr3());

    Gdt::get_instance().save();
    ExceptionTable::get_instance().register_exception_handlers();
    Idt::get_instance().save();

    auto& pic = Pic::get_instance();
    pic.remap();
    pic.disable();

    if (!Dmesg::get_instance().setup())
    {
        PANIC("Can't setup dmesg");
        return;
    }

    /* Until the command line has been read there is nothing to read it from,
       so the earliest traces are always at the default level. */
    Tracer::get_instance().set_level(Parameters::DEFAULT_LOG_LEVEL);

    //Screen::Printf("Hello!\n");

    grub::parse_multi_boot_info((grub::MultiBootInfoHeader *)bpt.phys_to_virt((ulong)mb_info));

    Tracer::get_instance().set_level(Parameters::get_instance().get_log_level());

    /* Microcode, as early as it can be: this CPU's update from the module
       GRUB loaded, while the bootstrap map still reaches it -- below 4 GiB,
       as a module's range is -- kept for the APs (ApMain2). */
    {
        ulong ucode_phys = 0, ucode_len = 0;
        if (grub::get_ucode_module(ucode_phys, ucode_len))
        {
            microcode::Result ucode;
            microcode::load_on_bsp((const u8*)bpt.phys_to_virt(ucode_phys), ucode_len, &ucode);
            if (ucode.skipped == nullptr)
                trace(0, "microcode: revision 0x%x -> 0x%x, of %04x-%02x-%02x", ucode.before, ucode.after,
                    ucode.date & microcode::DATE_YEAR_MASK, ucode.date >> microcode::DATE_MONTH_SHIFT,
                    (ucode.date >> microcode::DATE_DAY_SHIFT) & microcode::DATE_DAY_MASK);
            else if (ucode.asked)
                trace(0, "microcode: %s (revision 0x%x)", ucode.skipped, ucode.before);
            else
                trace(0, "microcode: %s", ucode.skipped);
        }
    }

    /* Arm log capture as soon as the command line is known: the ring buffers
       everything until the network can carry it away. */
    rust_netconsole_setup();

    auto& mmap = mm::MemoryMap::get_instance();
    trace(0, "Enter kernel: start 0x%lX end 0x%lX",
        mmap.get_kernel_start(), mmap.get_kernel_end());

    if (mmap.get_kernel_end() <= bpt.phys_to_virt(MB))
    {
        PANIC("Kernel end is lower than kernel space base");
        break;
    }

    /* Now that the firmware map is known, grow the bootstrap map over the
       RAM above 4GiB -- Setup() ran before the Multiboot tags were parsed
       and could not know there was any. */
    bpt.map_high_ram();

    auto& pt = mm::PageTable::get_instance();
    if (!pt.setup())
    {
        PANIC("Can't setup paging");
        break;
    }

    trace(0, "Paging root 0x%lX old cr3 0x%lX", pt.get_root(), get_cr3());
    set_cr3(pt.get_root());
    trace(0, "Set new cr3 0x%lX", get_cr3());
    if (!pt.setup_free_pages_list())
    {
        PANIC("Can't setup paging");
        break;
    }

    /* The huge pool (hugepages=), now: the free list will never again be as
       whole as it is before the kernel's own allocations begin. */
    pt.reserve_huge_pool(Parameters::get_instance().get_huge_pool_bytes(pt.get_total_pages_count() * consts::PAGE_SIZE));

    /* W^X: the image mapping is the one executable thing the page table
       was built with (everything else is already NX), so split it here into
       .text RX, .rodata RO+NX, .data/.bss RW+NX */
    {
        extern char kernel_start[], kernel_text[], kernel_rodata[], kernel_end[];
        /* Before the first MapMmioRegion: a write-combining mapping needs
           the PAT entry to exist. */
        hal::setup_memory_types();
        /* The firmware's choice, said once: the APs make the same one
           (ApMain2). */
        if (hal::setup_serializing_lfence())
            trace(0, "cpu: LFENCE made dispatch-serializing (DE_CFG bit 1 was clear)");
        /* The clock the CPU's own to pick, up to its turbo, rather than
           firmware's base: the APs do the same (ApMain2). */
        hal::CpuPerformance perf;
        if (hal::setup_cpu_performance(&perf))
            trace(0, "cpu: HWP on, performance %u to %u, %u guaranteed -- the clock the CPU's own pick",
                perf.lowest, perf.highest, perf.guaranteed);
        ulong t0 = (ulong)kernel_start, t1 = (ulong)kernel_text;
        ulong r1 = (ulong)kernel_rodata, e = (ulong)kernel_end;
        pt.protect_range(t0, t1 - t0, false, true);   /* RX */
        pt.protect_range(t1, r1 - t1, false, false);  /* RO+NX */
        pt.protect_range(r1, e - r1, true, false);    /* RW+NX */
        trace(0, "W^X: text [0x%lX,0x%lX) rodata [0x%lX,0x%lX) data [0x%lX,0x%lX)",
            t0, t1, t1, r1, r1, e);
        if (Parameters::get_instance().is_wx_probe_text())
        {
            trace(0, "W^X probe: writing to text 0x%lX (expect page fault)", t0);
            *reinterpret_cast<volatile u32*>(t0) = 0;
            trace(0, "W^X probe: text write SUCCEEDED (W^X broken!)");
        }
    }

    Gdt::get_instance().save();
    ExceptionTable::get_instance().register_exception_handlers();
    Idt::get_instance().save();

    //Test paging
    {
        trace(0, "Test paging");
        auto page = pt.alloc_page();
        if (!page) {
            PANIC("Can't alloc page");
            break;
        }
        auto va = pt.tmp_map_page(page->get_phy_address());
        trace(0, "va 0x%lX pha 0x%lX", va, page->get_phy_address());
        stdlib::mem_set((void *)va, 0, consts::PAGE_SIZE);
        pt.tmp_unmap_page(va);
        pt.free_page(page);
    }

    /* Pick the video console now that MMIO can be mapped: under UEFI the
       only screen is the pixel framebuffer GRUB hands over, and mapping it
       needs the page table (but not yet the heap). Must happen on the BSP
       before the APs start -- see MapMmioRegion. */
    screen::setup();

    screen::printf("Hello!\n");

    trace(0, "Parsing acpi...");

    auto& acpi = Acpi::get_instance();
    auto err = acpi.parse();
    if (!err.ok())
    {
        trace_error(err, "Can't parse ACPI");
        PANIC("Can't parse ACPI");
        break;
    }

    if (!mm::PageAllocatorImpl::get_instance().setup())
    {
        PANIC("Can't setup page allocator");
        break;
    }

    mm::AllocatorImpl::get_instance(&mm::PageAllocatorImpl::get_instance());

    if (Parameters::get_instance().is_wx_probe_heap())
        wx_probe_heap();

    /* Setup HPET after page allocator is ready (MMIO mapping requires it).
       HPET may not be available on all hardware; Setup() returns false silently. */
    Hpet::get_instance().setup();

    halt_tco_watchdog();

    /* Before anything can ask the pool for bytes: seeding needs no heap and
       no device, only the cycle counter and whatever instruction the cpu
       has, which the probe finds first. The devices that can do better are
       folded in by the rust_random_reseed() in BpStartup. */
    hal::probe_hw_random();
    if (!rust_random_setup())
        trace(0, "Random: unseeded at boot, https will fail until a source turns up");

    screen::printf("Self test begin, please wait...\n");

    trace(0, "Before test");

    err = test::test();
    if (!err.ok())
    {
        trace_error(err, "Test failed");
        PANIC("Self test failed");
        break;
    }

    trace(0, "After test");
    ubsan::announce();
    screen::printf("Self test complete, error %lu\n", (ulong)err.get_code());

    auto& pci = Pci::get_instance();
    pci.scan();

    IO8042::get_instance();
    auto& serial = Serial::get_instance();
    auto& cmd = Cmd::get_instance();
    auto& cpus = CpuTable::get_instance();
    auto& params = Parameters::get_instance();
    if (!params.is_console_serial())
    {
        /* One sink for every keyboard: the 8042 and the USB HID driver both
           publish into KeyboardInput. */
        if (!KeyboardInput::get_instance().register_observer(cmd))
        {
            PANIC("Can't register cmd in keyboard input");
            break;
        }
    }
    if (!params.is_console_vga())
    {
        if (!serial.register_observer(cmd))
        {
            PANIC("Can't register cmd in serial");
            break;
        }
    }

    Lapic::enable();

    auto& cpu = cpus.get_current_cpu();
    if (!cpus.set_bsp_index(cpu.get_index()))
    {
        PANIC("Can't set boot processor index");
        break;
    }

    trace(0, "Before cpu run");

    if (!cpu.run(bp_startup, nullptr))
    {
        PANIC("Can't run cpu %lu task", cpu.get_index());
        break;
    }

    } while (false);

    shutdown();
}

extern "C" void main_x86_64(grub::MultiBootInfoHeader *mb_info)
{
    per_cpu_setup_boot();

    ALLOC_CPU_STACK();
    main2(mb_info);
}

namespace kernel
{

void report_cpu_stacks(stdlib::Printer& printer, ulong& worst_free)
{
    /* These carry the deepest code in the kernel -- all of Main2 and the
       bring-up path run here, before the first task stack exists -- and
       unlike a task stack they have neither a magic word nor the one-page
       tripwire GetCurrentTask checks. */
    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        ulong free = stack_probe::untouched(&stack[i][0], CPU_STACK_SIZE);
        if (free == CPU_STACK_SIZE)
            continue;

        printer.printf("static %lu %lu %lu cpu-boot\n", i,
            (ulong)CPU_STACK_SIZE - free, (ulong)CPU_STACK_SIZE);

        if (free < worst_free)
            worst_free = free;
    }
}

}
