#include "pl011.h"
#include "board.h"
#include "gicv3.h"
#include "generic_timer.h"
#include "its.h"

#include <drivers/pci.h>

#include <lib/stdlib.h>
#include <mm/memory_map.h>
#include <mm/page_table.h>
#include <mm/page_allocator.h>
#include <mm/new.h>
#include <mm/allocator.h>
#include <hal/mmu.h>
#include <kernel/trace.h>
#include <kernel/panic.h>
#include <kernel/dmesg.h>
#include <kernel/parameters.h>
#include <kernel/ubsan.h>
#include <kernel/time.h>
#include <kernel/test.h>
#include <hal/random.h>
#include <kernel/cpu.h>
#include <kernel/preempt.h>
#include <kernel/cmd.h>
#include <kernel/softirq.h>
#include <kernel/stack_probe.h>
#include <hal/power.h>



extern "C" void rust_init();
/* The network layer is Rust (src/rust/net): the recycled frame pool, the
   log over UDP armed as soon as the command line is known, TCP, and the
   services boot starts once there is a device and a shell. */
extern "C" int rust_netframe_pool_setup(unsigned long count);
extern "C" int rust_netconsole_setup();
extern "C" int rust_tcp_init();
extern "C" void rust_net_start_services(unsigned short udp_shell_port);
extern "C" void rust_net_stop_services();
/* The filesystem layer is Rust (src/rust/fs): what boot mounts where, and
   taking it all down again on the way out. */
extern "C" void rust_mount_root_fs();
extern "C" void kernel_vfs_unmount_all();

/* The disk log is Rust (src/rust/block/src/disklog.rs). */
extern "C" int rust_disklog_setup();
extern "C" void rust_disklog_stop();
extern "C" void rust_test();
extern "C" void rust_fini();
/* The random pool is Rust (src/rust/random), the x86 twin's counterpart */
extern "C" bool rust_random_setup();
extern "C" void rust_random_reseed();
/* The partition table reader (src/rust/block), the x86 twin's counterpart */
extern "C" void rust_partitions_probe();

namespace kernel
{
/* Where a virtio-mmio window is, as the Rust drivers take it
   (virtio::mmio::Slot in src/rust/virtio/src/mmio.rs) */
struct VirtioMmioSlot
{
    ulong base;    /* kernel VA of the register window */
    ulong size;
    u32 int_id;
    u32 reserved;
};
}
/* The virtio-rng driver (src/rust/drivers/virtio_rng), which takes the
   virtio-mmio windows the device tree described -- the arm64 bus. */
extern "C" void rust_virtio_rng_init_mmio(const kernel::VirtioMmioSlot* slots, unsigned long count);
/* The virtio-blk driver (src/rust/drivers/virtio_blk), likewise over the
   windows the device tree described */
extern "C" void rust_virtio_blk_init_mmio(const kernel::VirtioMmioSlot* slots, unsigned long count);
/* The virtio-net driver (src/rust/drivers/virtio_net), likewise */
extern "C" void rust_virtio_net_init_mmio(const kernel::VirtioMmioSlot* slots, unsigned long count);
/* The virtio-scsi driver (src/rust/drivers/virtio_scsi), likewise */
extern "C" void rust_virtio_scsi_init_mmio(const kernel::VirtioMmioSlot* slots, unsigned long count);
namespace kernel { bool pci_ecam_setup(); }
extern "C" void __cxa_finalize(void*);
extern "C" char boot_stack_top[];
#include <kernel/module.h>

/* arm64 boot orchestrator, the Main2 twin (kernel/main.cpp). Milestone M2:
   full memory management + boot self-tests on one CPU; the interrupt/
   scheduler/driver stages follow in M3+ (plans/02-hal-arm64.md). */

namespace
{

const ulong MEM_REGION_USABLE_RAM = 1; /* e820-style type used by MemoryMap */
const ulong MEM_REGION_RESERVED = 2;

}

extern "C" char kernel_start[], kernel_text[], kernel_rodata[], kernel_end[];

namespace kernel
{
namespace mm
{
bool install_early_device_block(ulong real_root); /* arch/arm64/builtin_pt.cpp */
}

void setup_vectors(); /* arch/arm64/exception_arm64.cpp */

/* boot.S */
extern "C" char boot_stack[];
extern "C" char boot_stack_guard[];
extern "C" char boot_stack_top[];

typedef void (*HaltAction)();

/* Finalization context passed across the stack switch */
static Task* halt_task;
static HaltAction halt_action_fn;

/* Runs on the static boot stack: releases the idle task, runs static
   destructors and Rust teardown, then powers off/resets. Never returns
   (the x86 twin: finalize_on_static_stack in kernel/main.cpp). */
static void finalize_on_boot_stack(void* ctx)
{
    (void)ctx;

    trace(0, "finalize_on_boot_stack");

    halt_task->put();

    rust_fini();

    __cxa_finalize(0);

    trace(0, "Finalized");

    halt_action_fn();
}

static void __attribute__((noreturn)) prepare_halt_arm(HaltAction action)
{
    /* Extra ref so the idle task is not freed while we still run on its
       stack (released in finalize_on_boot_stack after the stack switch) */
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

    halt_task = task;
    halt_action_fn = action;
    hal::run_on_stack((ulong)&boot_stack_top[0], finalize_on_boot_stack, nullptr);
}

/* AP idle-task body (the x86 twin: ap_startup in kernel/main.cpp) */
static void ap_startup_arm(void* ctx)
{
    (void)ctx;

    auto& cpu = CpuTable::get_instance().get_current_cpu();

    trace(0, "Cpu %lu running task 0x%p", cpu.get_index(),
        Task::get_current_task());

    BUG_ON(hal::is_interrupt_enabled());

    /* Arm this CPU's own generic timer (per-CPU tick) before enabling IRQs */
    GenericTimer::get_instance().arm_self();

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

/* The bp_startup twin (kernel/main.cpp): runs as the BSP idle task */
static void bp_startup_arm(void* ctx)
{
    (void)ctx;

    auto& board = Board::get_instance();
    auto& cpu = CpuTable::get_instance().get_current_cpu();

    if (!GenericTimer::get_instance().setup())
    {
        PANIC("Can't setup timer");
        return;
    }

    /* The generic timer is per core by construction, and every CPU arms its
       own in ap_startup_arm -- so the tick work on the IPI path is a duplicate
       here, and has been since the port. */
    CpuTable::get_instance().set_per_cpu_timer();

    if (!Pl011::get_instance().setup(board.pl011_int_id_))
    {
        PANIC("Can't setup uart irq");
        return;
    }

    interrupt_enable();

    /* Hand the virtio-mmio windows the device tree described to the drivers
       (all inside the premapped device GiB). What is in each one is theirs
       to read; this only says where they are. Must run on the BSP before the
       APs start (map_mmio_region boot-ordering constraint). */
    {
        static VirtioMmioSlot slots[Board::MAX_VIRTIO_MMIO];
        ulong count = 0;
        for (ulong i = 0; i < board.virtio_mmio_count_; i++)
        {
            slots[count].base = mm::MemoryMap::KERNEL_SPACE_BASE + board.virtio_mmio_[i].base;
            slots[count].size = board.virtio_mmio_[i].size;
            slots[count].int_id = board.virtio_mmio_[i].int_id;
            slots[count].reserved = 0;
            count++;
        }
        trace(0, "virtio-mmio: %lu windows from the device tree", count);

        rust_virtio_blk_init_mmio(slots, count);
        rust_virtio_scsi_init_mmio(slots, count);
        /* 0 means the layer's own default. */
        rust_netframe_pool_setup(Parameters::get_instance().get_net_frame_count());

        rust_virtio_net_init_mmio(slots, count);
        rust_virtio_rng_init_mmio(slots, count);

        /* The virtio-rng carries the pool on this arch: no cpu here that the
           kernel runs on implements FEAT_RNG, Apple's included. */
        rust_random_reseed();
    }

    auto& cpus = CpuTable::get_instance();
    if (!cpus.start_all())
    {
        PANIC("Can't start all cpus");
        return;
    }

    preempt_on();
    trace(0, "Preempt is now on");

    /* IPI round-trip test (mirrors kernel/main.cpp bp_startup) */
    ulong cpu_mask = cpus.get_running_cpus();
    for (ulong i = 0; i < MAX_CPUS; i++)
    {
        if ((cpu_mask & (1UL << i)) && i != cpu.get_index())
            cpus.send_ipi(i);
    }

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

    trace(0, "After test");
    ubsan::announce();

    rust_init();
    rust_test();

    if (!SoftIrq::get_instance().init())
    {
        PANIC("Can't init softirq");
        return;
    }

    /* Partitions and the root filesystem: here, not next to the virtio
       bring-up, because a block request completes through the BLK_IO soft
       IRQ, and the NVMe disks came up in rust_init (see kernel/main.cpp) */
    rust_partitions_probe();
    rust_mount_root_fs();

    /* Here, and not earlier: a block request is completed through the BLK_IO
       soft IRQ, so until the line above a write returns without having
       written anything. Everything traced since the first line of the boot
       has been held in memory and goes down as soon as the area is found. */
    rust_disklog_setup();

    rust_tcp_init();

    auto& cmd = Cmd::get_instance();
    if (!Pl011::get_instance().register_observer(cmd))
    {
        PANIC("Can't register cmd in uart");
        return;
    }

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
            bool reboot = cmd.should_reboot();
            if (reboot)
                trace(0, "Reboot requested");
            else
                trace(0, "Shutdown requested");

            rust_net_stop_services();
            cmd.stop();
            cmd.stop_dhcp();

            /* With the shells gone nothing calls into a module any more:
               its exit runs now, while the files and block I/O it may still
               need are there (as in kernel/main.cpp) */
            ModuleTable::get_instance().unload_all();

            /* While the soft IRQs still run: unmounting writes the
               superblock, and a block request completes through BLK_IO */
            kernel_vfs_unmount_all();

            /* The disk log last of all: its writer finishes what is queued
               and the log switches off -- after SoftIrq::stop() a write
               through a virtio disk would wait for ever. */
            rust_disklog_stop();

            /* Then the soft IRQ tasks themselves, as on x86. Left running
               into the halt they were never released -- SoftIrq's own
               reference outlived it -- and one per CPU leaked on every
               poweroff. */
            SoftIrq::get_instance().stop();

            prepare_halt_arm(reboot ? hal::reset : hal::power_off);
        }
    }
}
}

/* wxprobe=heap: call into a page the allocator just handed out. Every
   runtime mapping is PXN, so the call must die on the instruction fetch; the
   line after it printing means W^X covers the kernel image and nothing else.
   Deliberately fatal, like the text probe -- it is asked for by name on the
   command line. The x86 twin is in kernel/main.cpp. */
static void wx_probe_heap()
{
    using namespace kernel;

    static const u32 RET_INSN = 0xD65F03C0; /* ret */

    ulong phys = 0;
    void* va = mm::alloc_map_pages(1, &phys);
    if (va == nullptr)
    {
        trace(0, "W^X probe: no page for the heap probe");
        return;
    }

    *reinterpret_cast<volatile u32*>(va) = RET_INSN;
    hal::sync_instruction_cache((ulong)va, sizeof(RET_INSN));
    trace(0, "W^X probe: calling into a heap page 0x%lX (expect instr abort)", (ulong)va);
    reinterpret_cast<void (*)()>(va)();
    trace(0, "W^X probe: heap execute SUCCEEDED (W^X broken!)");
}

/* PSCI CPU_ON lands here (via boot.S secondary_entry) on the AP boot stack */
extern "C" void ap_main_arm64(ulong index)
{
    using namespace kernel;

    setup_vectors();

    if (!Gic::get_instance().cpu_init())
        PANIC("Can't init gic on cpu %lu", index);

    auto& cpu = CpuTable::get_instance().get_current_cpu();
    BUG_ON(cpu.get_index() != index);

    if (!cpu.run(ap_startup_arm, nullptr))
    {
        trace(0, "Can't run cpu %lu task", cpu.get_index());
        return;
    }
}

extern "C" void main_arm64(void* dtb)
{
    using namespace kernel;

    setup_vectors();

    /* Fill the boot stack below the current stack pointer with the pattern
       whose survival says later how deep this got. Everything above the
       pointer is live and is left alone. */
    {
        ulong sp = hal::get_sp();
        ulong base = (ulong)&boot_stack[0];
        const ulong poison_margin = 2048;

        if (sp > base + poison_margin)
            stack_probe::poison(&boot_stack[0],
                (sp - base - poison_margin) & ~(sizeof(ulong) - 1));

        /* And the guard page below it, whole: CpuTable::start_all wants it
           untouched (boot.S). */
        stack_probe::poison(&boot_stack_guard[0], base - (ulong)&boot_stack_guard[0]);
    }

    auto& board = Board::get_instance();
    board.setup(dtb);

    /* Re-seed TPIDR_EL1 with this CPU's linear index in the DTB CPU list:
       boot.S could only guess MPIDR.Aff0, which matches the index on QEMU
       virt but not on clustered-affinity hardware. APs get the real index
       via the PSCI context argument, so only the BSP needs this. */
    {
        const ulong aff_mask = 0xFF00FFFFFFUL; /* Aff3:Aff2:Aff1:Aff0 */
        ulong mpidr;
        asm volatile("mrs %0, mpidr_el1" : "=r"(mpidr));
        for (ulong i = 0; i < board.cpu_count_; i++)
        {
            if (board.cpu_mpidr_[i] == (mpidr & aff_mask))
            {
                asm volatile("msr tpidr_el1, %0" :: "r"(i));
                break;
            }
        }
    }

    Pl011::early_init(mm::MemoryMap::KERNEL_SPACE_BASE + board.pl011_base_);
    Pl011::print_string("nos arm64: hello from EL1 (higher half)\n");

    if (!Dmesg::get_instance().setup())
    {
        Pl011::print_string("nos arm64: can't setup dmesg\n");
        for (;;) asm volatile("wfi");
    }

    /* Until the command line has been read there is nothing to read it from,
       so the earliest traces are always at the default level. */
    Tracer::get_instance().set_level(Parameters::DEFAULT_LOG_LEVEL);

    trace(0, "nos arm64: dtb 0x%p bootargs '%s' cpus %lu", dtb,
        board.boot_args_, board.cpu_count_);
    if (board.refused_ != 0)
        trace(0, "nos arm64: dtb: %lu values refused -- out of reach, not an interrupt the kernel "
            "takes, or unreadable -- the defaults in their place", board.refused_);

    auto& mmap = mm::MemoryMap::get_instance();
    for (ulong i = 0; i < board.mem_region_count_; i++)
    {
        mmap.add_region(board.mem_regions_[i].addr, board.mem_regions_[i].size,
            MEM_REGION_USABLE_RAM);
        trace(0, "memory 0x%lX size 0x%lX", board.mem_regions_[i].addr,
            board.mem_regions_[i].size);
    }
    /* Keep the allocator away from the DTB */
    if (board.dtb_region_.size != 0)
    {
        mmap.add_region(board.dtb_region_.addr - mm::MemoryMap::KERNEL_SPACE_BASE,
            board.dtb_region_.size, MEM_REGION_RESERVED);
    }

    Parameters::get_instance().parse(board.boot_args_);

    Tracer::get_instance().set_level(Parameters::get_instance().get_log_level());

    /* Arm log capture as soon as the command line is known: the ring buffers
       everything until the network can carry it away. */
    rust_netconsole_setup();

    trace(0, "Enter kernel: start 0x%lX end 0x%lX",
        mmap.get_kernel_start(), mmap.get_kernel_end());

    auto& bpt = mm::BuiltinPageTable::get_instance();
    if (!bpt.setup())
        PANIC("Can't setup builtin paging");

    bpt.map_high_ram();

    trace(0, "Builtin paging root 0x%lX", bpt.get_root());
    hal::set_translation_root(bpt.get_root());
    trace(0, "Builtin paging active");

    auto& pt = mm::PageTable::get_instance();
    if (!pt.setup())
        PANIC("Can't setup paging");

    trace(0, "Paging root 0x%lX", pt.get_root());

    /* Keep the UART (and the rest of the MMIO GiB) mapped across the
       root switch: install the device block while the builtin linear map
       is still active. */
    if (!mm::install_early_device_block(pt.get_root()))
        PANIC("Can't install device block");

    hal::set_translation_root(pt.get_root());
    trace(0, "Paging active");

    if (!pt.setup_free_pages_list())
        PANIC("Can't setup free pages list");

    /* The huge pool (hugepages=), now: the free list will never again be as
       whole as it is before the kernel's own allocations begin. */
    pt.reserve_huge_pool(Parameters::get_instance().get_huge_pool_bytes(pt.get_total_pages_count() * consts::PAGE_SIZE));

    /* W^X: kernel text is RX, everything else in the image is RW+NX.
       Runs after the free-pages list so the walk sees the final image
       mapping. The image is the only executable mapping the page table was
       built with -- every other one is NX from birth (map_range_locked). */
    {
        ulong text_start = (ulong)kernel_start;
        ulong text_end = (ulong)kernel_text;
        ulong rodata_end = (ulong)kernel_rodata;
        ulong img_end = (ulong)kernel_end;
        pt.protect_range(text_start, text_end - text_start, false, true);     /* RX */
        pt.protect_range(text_end, rodata_end - text_end, false, false);      /* RO+NX */
        pt.protect_range(rodata_end, img_end - rodata_end, true, false);      /* RW+NX */
        asm volatile("dsb ish; isb");
        trace(0, "W^X: text [0x%lX,0x%lX) rodata [0x%lX,0x%lX) data [0x%lX,0x%lX)",
            text_start, text_end, text_end, rodata_end, rodata_end, img_end);

        if (Parameters::get_instance().is_wx_probe_text())
        {
            trace(0, "W^X probe: writing to text 0x%lX (expect data abort)", text_start);
            *reinterpret_cast<volatile u32*>(text_start) = 0;
            trace(0, "W^X probe: text write SUCCEEDED (W^X broken!)");
        }
    }

    /* Test paging (mirrors Main2) */
    {
        trace(0, "Test paging");
        auto page = pt.alloc_page();
        if (!page)
            PANIC("Can't alloc page");
        auto va = pt.tmp_map_page(page->get_phy_address());
        trace(0, "va 0x%lX pha 0x%lX", va, page->get_phy_address());
        stdlib::mem_set((void *)va, 0, consts::PAGE_SIZE);
        pt.tmp_unmap_page(va);
        pt.free_page(page);
    }

    if (!mm::PageAllocatorImpl::get_instance().setup())
        PANIC("Can't setup page allocator");

    mm::AllocatorImpl::get_instance(&mm::PageAllocatorImpl::get_instance());

    if (Parameters::get_instance().is_wx_probe_heap())
        wx_probe_heap();

    time_init();

    /* Before anything can ask the pool for bytes: seeding needs no heap and
       no device, only the probe of the cpu's instruction first. The
       virtio-rng is folded in later, once the mmio slots have been probed. */
    hal::probe_hw_random();
    if (!rust_random_setup())
        trace(0, "Random: unseeded at boot, https will fail until a source turns up");

    trace(0, "Before test");

    auto err = test::test();
    if (!err.ok())
    {
        trace_error(err, "Test failed");
        PANIC("Self test failed");
    }

    trace(0, "Self test passed");

    auto& gic = Gic::get_instance();
    if (!gic.setup(board.gicd_base_, board.gicr_base_, board.gicr_size_))
        PANIC("Can't setup gic");

    /* MSI via the ITS (best-effort: virtio-mmio works without it) */
    if (board.its_base_ != 0 && Parameters::get_instance().is_its_enabled())
    {
        if (!Its::get_instance().setup(board.its_base_, board.gicr_base_, board.gicr_size_))
            trace(0, "ITS setup failed; PCIe MSI unavailable");
    }

    /* PCIe over ECAM: map config space, enumerate, assign BARs */
    if (pci_ecam_setup())
    {
        Pci::get_instance().scan();
    }

    auto& cpus = CpuTable::get_instance();
    for (ulong i = 0; i < board.cpu_count_ && i < MAX_CPUS; i++)
    {
        if (!cpus.insert_cpu(i))
            PANIC("Can't insert cpu");
    }

    auto& cpu = cpus.get_current_cpu();
    if (!cpus.set_bsp_index(cpu.get_index()))
        PANIC("Can't set boot processor index");

    trace(0, "Before cpu run");

    if (!cpu.run(bp_startup_arm, nullptr))
        PANIC("Can't run cpu task");

    for (;;)
    {
        asm volatile("wfi");
    }
}
