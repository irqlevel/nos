#include "trace.h"
#include "panic.h"
#include "debug.h"
#include "atomic.h"
#include "gdt.h"
#include "idt.h"
#include "test.h"
#include "exception.h"
#include "asm.h"
#include "cpu.h"
#include "cmd.h"
#include "interrupt.h"
#include "icxxabi.h"
#include "preempt.h"
#include "dmesg.h"
#include "watchdog.h"
#include "parameters.h"
#include "console.h"
#include "softirq.h"
#include "time.h"

#include <boot/grub.h>

#include <lib/error.h>
#include <lib/stdlib.h>

#include <mm/new.h>
#include <mm/page_allocator.h>
#include <mm/memory_map.h>
#include <mm/allocator.h>
#include <mm/page_table.h>

#include <drivers/8042.h>
#include <drivers/vga.h>
#include <drivers/serial.h>
#include <drivers/pic.h>
#include <drivers/pit.h>
#include <drivers/acpi.h>
#include <drivers/lapic.h>
#include <drivers/ioapic.h>
#include <drivers/pci.h>
#include <drivers/virtio_blk.h>
#include <drivers/virtio_scsi.h>
#include <drivers/virtio_net.h>
#include <drivers/virtio_rng.h>

#include <block/block_device.h>
#include <block/partition.h>
#include <net/udp_shell.h>
#include <net/tcp.h>
#include <fs/vfs.h>
#include <fs/ramfs.h>
#include <fs/ext2.h>
#include <fs/nanofs.h>
#include <fs/procfs.h>

using namespace Kernel;
using namespace Stdlib;
using namespace Const;

const size_t CpuStackSize = 8 * Const::PageSize;
static char Stack[MaxCpus][8 * Const::PageSize] __attribute__((aligned(Const::PageSize)));
static long StackIndex;
static volatile long ApStartedFlag;

#define ALLOC_CPU_STACK()                               \
do {                                                    \
    auto index = AtomicReadAndInc(&StackIndex);         \
    if (index >= (long)MaxCpus)                         \
        Panic("Can't allocate stack for cpu");          \
    SetRsp((long)&Stack[index][CpuStackSize-128]);      \
} while (false)

void TraceCpuState(ulong cpu)
{
    Trace(0, "Cpu %u cr0 0x%p cr2 0x%p cr3 0x%p cr4 0x%p",
        cpu, GetCr0(), GetCr2(), GetCr3(), GetCr4());

    Trace(0, "Cpu %u rflags 0x%p rsp 0x%p rip 0x%p",
        cpu, GetRflags(), GetRsp(), GetRip());

    Trace(0, "Cpu %u ss 0x%p cs 0x%p ds 0x%p gs 0x%p fs 0x%p es 0x%p",
        cpu, (ulong)GetSs(), (ulong)GetCs(), (ulong)GetDs(),
        (ulong)GetGs(), (ulong)GetFs(), (ulong)GetEs());
}

void ApStartup(void *ctx)
{
    (void)ctx;

    auto& cpu = CpuTable::GetInstance().GetCurrentCpu();

    Trace(0, "Cpu %u running rflags 0x%p task 0x%p",
        cpu.GetIndex(), GetRflags(), Task::GetCurrentTask());

    TraceCpuState(cpu.GetIndex());

    BugOn(IsInterruptEnabled());
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

void ApMain2()
{
    AtomicReadAndInc(&ApStartedFlag); /* 2: entered ApMain2 */

    SetCr3(Mm::PageTable::GetInstance().GetRoot());
    AtomicReadAndInc(&ApStartedFlag); /* 3: page tables loaded */

    Gdt::GetInstance().Save();
    Idt::GetInstance().Save();
    AtomicReadAndInc(&ApStartedFlag); /* 4: GDT+IDT loaded */

    if (Parameters::GetInstance().IsSmpOff())
        Panic("AP cpu started while smp is off");

    Lapic::Enable();
    AtomicReadAndInc(&ApStartedFlag); /* 5: LAPIC enabled */

    auto& cpu = CpuTable::GetInstance().GetCurrentCpu();

    Trace(0, "Cpu %u rsp 0x%p (static stack)", cpu.GetIndex(), GetRsp());

    if (!cpu.Run(ApStartup, nullptr))
    {
        Trace(0, "Can't run cpu %u task", cpu.GetIndex());
        return;
    }
}

extern "C" void ApMain()
{
    AtomicReadAndInc(&ApStartedFlag);
    ALLOC_CPU_STACK();
    ApMain2();
}

typedef void (*HaltAction)();

/* Runs entirely on the static stack with its own stack frame.
   Releases the idle task, runs static destructors, then
   performs the final halt/reboot action. Never returns. */
static void __attribute__((noinline, noreturn))
FinalizeOnStaticStack(Task* task, HaltAction action)
{
    Trace(0, "FinalizeOnStaticStack");

    task->Put();

    __cxa_finalize(0);

    Trace(0, "Finalized");

    action();

    while (1) Hlt();
}

static void PrepareHalt(HaltAction action)
{
    /* Hold an extra reference so CpuTable::Reset()'s Put()
    only drops refcount 2→1 instead of freeing the task
    while we are still running on its stack. */
    auto task = Task::GetCurrentTask();
    task->Get();

    Vfs::GetInstance().UnmountAll();

    PreemptDisable();

    Trace(0, "Stopping cpu's");

    CpuTable::GetInstance().ExitAllExceptSelf();

    Trace(0, "Cpu's stopped");

    PreemptOff();

    CpuTable::GetInstance().Reset();
    Dmesg::GetInstance().Reset();

    InterruptDisable();

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
    SetRsp((long)&Stack[0][CpuStackSize - 128]);
    FinalizeOnStaticStack(task, action);
}

static void __attribute__((noreturn)) DoShutdown()
{
    Trace(0, "ACPI shutdown");

    /* ACPI S5 (soft-off): write SLP_TYP | SLP_EN to PM1a_CNT.
       QEMU/KVM uses PM1a_CNT port 0x604, SLP_TYP=0 for S5. */
    Outw(0x604, (1 << 13));

    /* Fallback: QEMU debug exit device */
    Outb(0xf4, 0x0);

    while (1) Hlt();
}

static void __attribute__((noreturn)) DoReboot()
{
    Trace(0, "Reboot");

    /* Keyboard controller reset (pulse CPU reset line) */
    Outb(0x64, 0xFE);

    /* Fallback: PCI reset register */
    Outb(0xCF9, 0x06);

    while (1) Hlt();
}

void Shutdown()
{
    auto& con = Console::GetInstance();
    con.Printf("Shutting down!\n");

    PrepareHalt(DoShutdown);
}

void Reboot()
{
    auto& con = Console::GetInstance();
    con.Printf("Rebooting!\n");

    PrepareHalt(DoReboot);
}

void SomeTaskRoutine(void *ctx)
{
    (void)ctx;

    auto task = Task::GetCurrentTask();
    while (!task->IsStopping())
        Sleep(100 * Const::NanoSecsInMs);
}

void MountRootFs()
{
    if (!Parameters::GetInstance().IsRootAuto())
        return;

    auto& vfs = Vfs::GetInstance();

    RamFs* ramfs = new RamFs();
    if (ramfs == nullptr || !vfs.Mount("/", ramfs))
    {
        Trace(0, "MountRootFs: failed to mount ramfs on /");
        delete ramfs;
        return;
    }
    Trace(0, "MountRootFs: mounted ramfs on / (rw)");

    vfs.CreateDir("/boot");

    auto& table = BlockDeviceTable::GetInstance();
    for (ulong i = 0; i < table.GetCount(); i++)
    {
        BlockDevice* dev = table.GetDevice(i);
        Ext2Fs* ext2 = new Ext2Fs(dev);
        if (ext2 != nullptr && vfs.Mount("/boot", ext2, true))
        {
            Trace(0, "MountRootFs: mounted ext2 on /boot from %s (ro)",
                dev->GetName());
            break;
        }
        delete ext2;
    }

    vfs.CreateDir("/data");

    for (ulong i = 0; i < table.GetCount(); i++)
    {
        BlockDevice* dev = table.GetDevice(i);
        NanoFs* nanofs = new NanoFs(dev);
        if (nanofs != nullptr && vfs.Mount("/data", nanofs))
        {
            Trace(0, "MountRootFs: mounted nanofs on /data from %s (rw)",
                dev->GetName());
            break;
        }
        delete nanofs;
    }

    vfs.CreateDir("/proc");

    ProcFs* procfs = new ProcFs();
    if (procfs != nullptr && vfs.Mount("/proc", procfs, true))
    {
        Trace(0, "MountRootFs: mounted procfs on /proc (ro)");
    }
    else
    {
        delete procfs;
    }
}

/* Shutdown()/Reboot() end up in PrepareHalt which abandons this
   stack via SetRsp without unwinding (see the WARNING there).
   All C++ objects with non-trivial destructors must go out of
   scope before those calls, so the body is wrapped in a block. */
void BpStartup(void* ctx)
{
    (void)ctx;

    bool doReboot = false;

    {
        auto& idt = Idt::GetInstance();
        auto& pit = Pit::GetInstance();
        auto& kbd = IO8042::GetInstance();
        auto& serial = Serial::GetInstance();
        auto& cmd = Cmd::GetInstance();
        auto& ioApic = IoApic::GetInstance();
        auto& cpus = CpuTable::GetInstance();
        auto& cpu = cpus.GetCurrentCpu();
        auto& acpi = Acpi::GetInstance();

        Trace(0, "Cpu %u running rflags 0x%p task 0x%p",
            cpu.GetIndex(), GetRflags(), Task::GetCurrentTask());

        TraceCpuState(cpu.GetIndex());

        ioApic.Enable();

        // PIT is ISA IRQ 0; ACPI MADT may remap it to GSI 2
        Interrupt::Register(pit, acpi.GetGsiByIrq(0x0), 0x20);
        Interrupt::Register(kbd, acpi.GetGsiByIrq(0x1), 0x21);
        Interrupt::Register(serial, acpi.GetGsiByIrq(0x4), 0x24);

        VirtioBlk::InitAll();
        VirtioScsi::InitAll();
        PartitionDevice::ProbeAll();
        MountRootFs();
        VirtioNet::InitAll();
        VirtioRng::InitAll();

        Trace(0, "Interrupts registered");

        idt.SetDescriptor(CpuTable::IPIVector, IdtDescriptor::Encode(IPInterruptStub));

        Trace(0, "IPI registred");

        idt.Save();

        Trace(0, "Idt saved");

        Trace(0, "Interrupts enabled %u", (ulong)IsInterruptEnabled());

        BugOn(IsInterruptEnabled());

        Trace(0, "Before pit setup");

        pit.Setup();

        Trace(0, "Before interrupt enable");

        InterruptEnable();

        BlockDevice::SetInterruptsStarted();

        Trace(0, "Interrupts enabled %u", (ulong)IsInterruptEnabled());

        /*
         * TSC calibration uses PIT channel 2 with busy-polling (~150ms).
         * Done after InterruptEnable so PIT channel 0 interrupts flow
         * normally, and before cpus.StartAll so the clock source is ready
         * for per-CPU time queries.
         */
        TimeInit();

        Trace(0, "Before cpus start");

        if (!Parameters::GetInstance().IsSmpOff())
        {
            if (!cpus.StartAll())
            {
                /* Diagnostic: did the AP reach ApMain()? */
                Trace(0, "AP diag: ApStartedFlag=%u",
                      (ulong)ApStartedFlag);
                Panic("Can't start all cpus");
                return;
            }
        }

        Trace(0, "Before preempt on");

        PreemptOn();

        Trace(0, "Preempt is now on");

        VgaTerm::GetInstance().Printf("IPI test...\n");

        ulong cpuMask = cpus.GetRunningCpus();
        for (ulong i = 0; i < MaxCpus; i++)
        {
            if (cpuMask & (1UL << i))
            {
                if (i != cpu.GetIndex())
                {
                    cpus.SendIPI(i);
                }
            }
        }

        VgaTerm::GetInstance().Printf("Task test...\n");

        if (!Test::TestMultiTasking())
        {
            Panic("Multitasking test failed");
            return;
        }

        if (!SoftIrq::GetInstance().Init())
        {
            Panic("Can't init softirq");
            return;
        }

        Tcp::GetInstance().Init();

        VgaTerm::GetInstance().Printf("Idle looping...\n");

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

        for (;;)
        {
            cpu.Idle();
            if (cmd.ShouldShutdown() || cmd.ShouldReboot())
            {
                doReboot = cmd.ShouldReboot();
                if (doReboot)
                    Trace(0, "Reboot requested");
                else
                    Trace(0, "Shutdown requested");
                udpShell.Stop();
                cmd.Stop();
                cmd.StopDhcp();
                break;
            }
        }

        SoftIrq::GetInstance().Stop();
    } /* all locals destroyed before stack is abandoned */

    if (doReboot)
        Reboot();
    else
        Shutdown();
}

void Main2(Grub::MultiBootInfoHeader *MbInfo)
{
    do {

    Panicker::GetInstance();
    Watchdog::GetInstance();

    Trace(0, "Cpu rsp 0x%p rbp 0x%p", GetRsp(), GetRbp());
    for (ulong i = 0; i < MaxCpus; i++)
    {
        Trace(0, "Static stack[%u] base 0x%p top 0x%p",
            i, (ulong)&Stack[i][0], (ulong)&Stack[i][CpuStackSize]);
    }

    auto& bpt = Mm::BuiltinPageTable::GetInstance();
    if (!bpt.Setup())
    {
        Panic("Can't setup paging");
        break;
    }

    Trace(0, "Paging root 0x%p old cr3 0x%p", bpt.GetRoot(), GetCr3());
    SetCr3(bpt.GetRoot());
    Trace(0, "Set new cr3 0x%p", GetCr3());

    Gdt::GetInstance().Save();
    ExceptionTable::GetInstance().RegisterExceptionHandlers();
    Idt::GetInstance().Save();

    auto& pic = Pic::GetInstance();
    pic.Remap();
    pic.Disable();

    if (!Dmesg::GetInstance().Setup())
    {
        Panic("Can't setup dmesg");
        return;
    }

    Tracer::GetInstance().SetLevel(1);

    //VgaTerm::GetInstance().Printf("Hello!\n");

    Grub::ParseMultiBootInfo((Grub::MultiBootInfoHeader *)bpt.PhysToVirt((ulong)MbInfo));

    auto& mmap = Mm::MemoryMap::GetInstance();
    Trace(0, "Enter kernel: start 0x%p end 0x%p",
        mmap.GetKernelStart(), mmap.GetKernelEnd());

    if (mmap.GetKernelEnd() <= bpt.PhysToVirt(MB))
    {
        Panic("Kernel end is lower than kernel space base");
        break;
    }

    auto& pt = Mm::PageTable::GetInstance();
    if (!pt.Setup())
    {
        Panic("Can't setup paging");
        break;
    }

    Trace(0, "Paging root 0x%p old cr3 0x%p", pt.GetRoot(), GetCr3());
    SetCr3(pt.GetRoot());
    Trace(0, "Set new cr3 0x%p", GetCr3());
    if (!pt.SetupFreePagesList())
    {
        Panic("Can't setup paging");
        break;
    }

    Gdt::GetInstance().Save();
    ExceptionTable::GetInstance().RegisterExceptionHandlers();
    Idt::GetInstance().Save();

    //Test paging
    {
        Trace(0, "Test paging");
        auto page = pt.AllocPage();
        if (!page) {
            Panic("Can't alloc page");
            break;
        }
        auto va = pt.TmpMapPage(page->GetPhyAddress());
        Trace(0, "va 0x%p pha 0x%p", va, page->GetPhyAddress());
        Stdlib::MemSet((void *)va, 0, Const::PageSize);
        pt.TmpUnmapPage(va);
        pt.FreePage(page);
    }

    VgaTerm::GetInstance().Printf("Hello!\n");

    Trace(0, "Parsing acpi...");

    auto& acpi = Acpi::GetInstance();
    auto err = acpi.Parse();
    if (!err.Ok())
    {
        TraceError(err, "Can't parse ACPI");
        Panic("Can't parse ACPI");
        break;
    }

    if (!Mm::PageAllocatorImpl::GetInstance().Setup())
    {
        Panic("Can't setup page allocator");
        break;
    }

    Mm::AllocatorImpl::GetInstance(&Mm::PageAllocatorImpl::GetInstance());

    VgaTerm::GetInstance().Printf("Self test begin, please wait...\n");

    Trace(0, "Before test");

    err = Test::Test();
    if (!err.Ok())
    {
        TraceError(err, "Test failed");
        Panic("Self test failed");
        break;
    }

    Trace(0, "After test");
    VgaTerm::GetInstance().Printf("Self test complete, error %u\n", (ulong)err.GetCode());

    auto& pci = Pci::GetInstance();
    pci.Scan();

    auto& kbd = IO8042::GetInstance();
    auto& serial = Serial::GetInstance();
    auto& cmd = Cmd::GetInstance();
    auto& cpus = CpuTable::GetInstance();
    auto& params = Parameters::GetInstance();
    if (!params.IsConsoleSerial())
    {
        if (!kbd.RegisterObserver(cmd))
        {
            Panic("Can't register cmd in kbd");
            break;
        }
    }
    if (!params.IsConsoleVga())
    {
        if (!serial.RegisterObserver(cmd))
        {
            Panic("Can't register cmd in serial");
            break;
        }
    }

    Lapic::Enable();

    auto& cpu = cpus.GetCurrentCpu();
    if (!cpus.SetBspIndex(cpu.GetIndex()))
    {
        Panic("Can't set boot processor index");
        break;
    }

    Trace(0, "Before cpu run");

    if (!cpu.Run(BpStartup, nullptr))
    {
        Panic("Can't run cpu %u task", cpu.GetIndex());
        break;
    }

    } while (false);

    Shutdown();
}

extern "C" void Main(Grub::MultiBootInfoHeader *MbInfo)
{
    ALLOC_CPU_STACK();
    Main2(MbInfo);
}