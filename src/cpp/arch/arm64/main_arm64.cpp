#include "pl011.h"
#include "board.h"
#include "gicv3.h"
#include "generic_timer.h"

#include <lib/stdlib.h>
#include <mm/memory_map.h>
#include <mm/page_table.h>
#include <mm/page_allocator.h>
#include <mm/allocator.h>
#include <hal/mmu.h>
#include <kernel/trace.h>
#include <kernel/panic.h>
#include <kernel/dmesg.h>
#include <kernel/parameters.h>
#include <kernel/time.h>
#include <kernel/test.h>
#include <kernel/cpu.h>
#include <kernel/preempt.h>
#include <kernel/cmd.h>
#include <kernel/softirq.h>
#include <hal/power.h>

#include <drivers/virtio_mmio.h>
#include <drivers/virtio_blk.h>
#include <drivers/virtio_net.h>
#include <drivers/virtio_rng.h>

#include <net/tcp.h>
#include <net/udp_shell.h>
#include <net/net_device.h>

/* arm64 boot orchestrator, the Main2 twin (kernel/main.cpp). Milestone M2:
   full memory management + boot self-tests on one CPU; the interrupt/
   scheduler/driver stages follow in M3+ (plans/02-hal-arm64.md). */

namespace
{

const ulong MemRegionUsableRam = 1; /* e820-style type used by MemoryMap */
const ulong MemRegionReserved = 2;

}

namespace Kernel
{
namespace Mm
{
bool InstallEarlyDeviceBlock(ulong realRoot); /* arch/arm64/builtin_pt.cpp */
}

void SetupVectors(); /* arch/arm64/exception_arm64.cpp */

/* AP idle-task body (the x86 twin: ApStartup in kernel/main.cpp) */
static void ApStartupArm(void* ctx)
{
    (void)ctx;

    auto& cpu = CpuTable::GetInstance().GetCurrentCpu();

    Trace(0, "Cpu %u running task 0x%p", cpu.GetIndex(),
        Task::GetCurrentTask());

    BugOn(Hal::IsInterruptEnabled());
    InterruptEnable();

    cpu.SetRunning();

    PreemptOnWait();

    if (!Test::TestMultiTasking())
    {
        Panic("Multitasking test failed");
        return;
    }

    for (;;)
    {
        cpu.Idle();
    }
}

/* The BpStartup twin (kernel/main.cpp): runs as the BSP idle task */
static void BpStartupArm(void* ctx)
{
    (void)ctx;

    auto& board = Board::GetInstance();
    auto& cpu = CpuTable::GetInstance().GetCurrentCpu();

    if (!GenericTimer::GetInstance().Setup())
    {
        Panic("Can't setup timer");
        return;
    }

    if (!Pl011::GetInstance().Setup(board.Pl011IntId))
    {
        Panic("Can't setup uart irq");
        return;
    }

    InterruptEnable();

    /* Discover virtio-mmio devices from the DTB slots (all inside the
       premapped device GiB). Must run on the BSP before the APs start
       (MapMmioRegion boot-ordering constraint). */
    {
        static VirtioMmioSlot Slots[Board::MaxVirtioMmio];
        ulong count = 0;
        for (ulong i = 0; i < board.VirtioMmioCount; i++)
        {
            ulong va = Mm::MemoryMap::KernelSpaceBase + board.VirtioMmio[i].Base;
            u32 devId = VirtioMmio::ReadDeviceId(va);
            if (devId == 0)
                continue;
            Slots[count].Base = va;
            Slots[count].Size = board.VirtioMmio[i].Size;
            Slots[count].IntId = board.VirtioMmio[i].IntId;
            Slots[count].DeviceId = devId;
            count++;
        }
        Trace(0, "virtio-mmio: %u devices", count);

        VirtioBlk::InitAllMmio(Slots, count);
        VirtioNet::InitAllMmio(Slots, count);
        VirtioRng::InitAllMmio(Slots, count);
    }

    auto& cpus = CpuTable::GetInstance();
    if (!cpus.StartAll())
    {
        Panic("Can't start all cpus");
        return;
    }

    PreemptOn();
    Trace(0, "Preempt is now on");

    /* IPI round-trip test (mirrors kernel/main.cpp BpStartup) */
    ulong cpuMask = cpus.GetRunningCpus();
    for (ulong i = 0; i < MaxCpus; i++)
    {
        if ((cpuMask & (1UL << i)) && i != cpu.GetIndex())
            cpus.SendIPI(i);
    }

    if (!Test::TestMultiTasking())
    {
        Panic("Multitasking test failed");
        return;
    }

    Trace(0, "After test");

    if (!SoftIrq::GetInstance().Init())
    {
        Panic("Can't init softirq");
        return;
    }

    Tcp::GetInstance().Init();

    auto& cmd = Cmd::GetInstance();
    if (!Pl011::GetInstance().RegisterObserver(cmd))
    {
        Panic("Can't register cmd in uart");
        return;
    }

    if (!cmd.Start())
    {
        Panic("Can't start cmd");
        return;
    }

    UdpShell udpShell;
    u16 udpShellPort = Parameters::GetInstance().GetUdpShellPort();
    if (udpShellPort != 0)
    {
        NetDevice* netDev = NetDeviceTable::GetInstance().Find("eth0");
        if (netDev)
        {
            if (!udpShell.Start(netDev, udpShellPort))
                Trace(0, "UdpShell: failed to start on port %u", (ulong)udpShellPort);
        }
        else
        {
            Trace(0, "UdpShell: eth0 not found");
        }
    }

    Trace(0, "boot: complete");

    for (;;)
    {
        cpu.Idle();
        if (cmd.ShouldShutdown() || cmd.ShouldReboot())
        {
            /* Graceful task/fs teardown mirrors main.cpp later; for now
               go straight to PSCI */
            if (cmd.ShouldReboot())
            {
                Trace(0, "Reboot requested");
                Hal::Reset();
            }
            else
            {
                Trace(0, "Shutdown requested");
                Hal::PowerOff();
            }
        }
    }
}
}

/* PSCI CPU_ON lands here (via boot.S SecondaryEntry) on the AP boot stack */
extern "C" void ApMainArm64(ulong index)
{
    using namespace Kernel;

    SetupVectors();

    if (!Gic::GetInstance().CpuInit())
        Panic("Can't init gic on cpu %u", index);

    auto& cpu = CpuTable::GetInstance().GetCurrentCpu();
    BugOn(cpu.GetIndex() != index);

    if (!cpu.Run(ApStartupArm, nullptr))
    {
        Trace(0, "Can't run cpu %u task", cpu.GetIndex());
        return;
    }
}

extern "C" void MainArm64(void* dtb)
{
    using namespace Kernel;

    SetupVectors();

    auto& board = Board::GetInstance();
    board.Setup(dtb);

    Pl011::EarlyInit(Mm::MemoryMap::KernelSpaceBase + board.Pl011Base);
    Pl011::PrintString("nos arm64: hello from EL1 (higher half)\n");

    if (!Dmesg::GetInstance().Setup())
    {
        Pl011::PrintString("nos arm64: can't setup dmesg\n");
        for (;;) asm volatile("wfi");
    }

    Tracer::GetInstance().SetLevel(1);

    Trace(0, "nos arm64: dtb 0x%p bootargs '%s' cpus %u", dtb,
        board.BootArgs, board.CpuCount);

    auto& mmap = Mm::MemoryMap::GetInstance();
    for (ulong i = 0; i < board.MemRegionCount; i++)
    {
        mmap.AddRegion(board.MemRegions[i].Addr, board.MemRegions[i].Size,
            MemRegionUsableRam);
        Trace(0, "memory 0x%p size 0x%p", board.MemRegions[i].Addr,
            board.MemRegions[i].Size);
    }
    /* Keep the allocator away from the DTB */
    if (board.DtbRegion.Size != 0)
    {
        mmap.AddRegion(board.DtbRegion.Addr - Mm::MemoryMap::KernelSpaceBase,
            board.DtbRegion.Size, MemRegionReserved);
    }

    Parameters::GetInstance().Parse(board.BootArgs);

    Trace(0, "Enter kernel: start 0x%p end 0x%p",
        mmap.GetKernelStart(), mmap.GetKernelEnd());

    auto& bpt = Mm::BuiltinPageTable::GetInstance();
    if (!bpt.Setup())
        Panic("Can't setup builtin paging");

    Trace(0, "Builtin paging root 0x%p", bpt.GetRoot());
    Hal::SetTranslationRoot(bpt.GetRoot());
    Trace(0, "Builtin paging active");

    auto& pt = Mm::PageTable::GetInstance();
    if (!pt.Setup())
        Panic("Can't setup paging");

    Trace(0, "Paging root 0x%p", pt.GetRoot());

    /* Keep the UART (and the rest of the MMIO GiB) mapped across the
       root switch: install the device block while the builtin linear map
       is still active. */
    if (!Mm::InstallEarlyDeviceBlock(pt.GetRoot()))
        Panic("Can't install device block");

    Hal::SetTranslationRoot(pt.GetRoot());
    Trace(0, "Paging active");

    if (!pt.SetupFreePagesList())
        Panic("Can't setup free pages list");

    /* Test paging (mirrors Main2) */
    {
        Trace(0, "Test paging");
        auto page = pt.AllocPage();
        if (!page)
            Panic("Can't alloc page");
        auto va = pt.TmpMapPage(page->GetPhyAddress());
        Trace(0, "va 0x%p pha 0x%p", va, page->GetPhyAddress());
        Stdlib::MemSet((void *)va, 0, Const::PageSize);
        pt.TmpUnmapPage(va);
        pt.FreePage(page);
    }

    if (!Mm::PageAllocatorImpl::GetInstance().Setup())
        Panic("Can't setup page allocator");

    Mm::AllocatorImpl::GetInstance(&Mm::PageAllocatorImpl::GetInstance());

    TimeInit();

    Trace(0, "Before test");

    auto err = Test::Test();
    if (!err.Ok())
    {
        TraceError(err, "Test failed");
        Panic("Self test failed");
    }

    Trace(0, "Self test passed");

    auto& gic = Gic::GetInstance();
    if (!gic.Setup(board.GicdBase, board.GicrBase, board.GicrSize))
        Panic("Can't setup gic");

    auto& cpus = CpuTable::GetInstance();
    for (ulong i = 0; i < board.CpuCount && i < MaxCpus; i++)
    {
        if (!cpus.InsertCpu(i))
            Panic("Can't insert cpu");
    }

    auto& cpu = cpus.GetCurrentCpu();
    if (!cpus.SetBspIndex(cpu.GetIndex()))
        Panic("Can't set boot processor index");

    Trace(0, "Before cpu run");

    if (!cpu.Run(BpStartupArm, nullptr))
        Panic("Can't run cpu task");

    for (;;)
    {
        asm volatile("wfi");
    }
}
