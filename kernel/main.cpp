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

using namespace Kernel;
using namespace Stdlib;
using namespace Const;

const size_t CpuStackSize = 8 * Const::PageSize;
static char Stack[MaxCpus][8 * Const::PageSize] __attribute__((aligned(Const::PageSize)));
static long StackIndex;

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
        Panic("Mulitasking test failed");
        return;
    }

    for (;;)
    {
        cpu.Idle();
    }
}

void ApMain2()
{
    SetCr3(Mm::PageTable::GetInstance().GetRoot());

    Gdt::GetInstance().Save();
    Idt::GetInstance().Save();

    if (Parameters::GetInstance().IsSmpOff())
        Panic("AP cpu started while smp is off");

    Lapic::Enable();

    auto& cpu = CpuTable::GetInstance().GetCurrentCpu();

    Trace(0, "Cpu %u rsp 0x%p", cpu.GetIndex(), GetRsp());

    if (!cpu.Run(ApStartup, nullptr))
    {
        Trace(0, "Can't run cpu %u task", cpu.GetIndex());
        return;
    }
}

extern "C" void ApMain()
{
    ALLOC_CPU_STACK();
    ApMain2();
}

void Shutdown()
{
    PreemptDisable();

    VgaTerm::GetInstance().Printf("Shutting down!\n");

    Trace(0, "Stopping cpu's");

    CpuTable::GetInstance().ExitAllExceptSelf();

    Trace(0, "Cpu's stopped");

    PreemptOff();

    CpuTable::GetInstance().Reset();
    Dmesg::GetInstance().Reset();

    Trace(0, "Bye");
    VgaTerm::GetInstance().Printf("Bye!\n");

    //Prevent this idle0 task from destruction
    //due too we now run inside it stack
    auto task = Task::GetCurrentTask();
    task->Get();

    InterruptDisable();

    __cxa_finalize(0);

    Trace(0, "Finalized");

    //Notify QEMU about shutdown through it's run with "-device isa-debug-exit,iobase=0xf4,iosize=0x04"
    Outb(0xf4, 0x0);
    Hlt();
}

void SomeTaskRoutine(void *ctx)
{
    (void)ctx;

    auto task = Task::GetCurrentTask();
    while (!task->IsStopping())
        Sleep(100 * Const::NanoSecsInMs);
}

static const ulong Tag = 'Main';
static Task* SomeTasks[10];

void StartSomeTasks()
{
    for (size_t i = 0; i < Stdlib::ArraySize(SomeTasks); i++)
    {
        SomeTasks[i] = Mm::TAlloc<Task, Tag>("SomeTask%u", i);
        if (SomeTasks[i] == nullptr)
        {
            Panic("Can't create task");
            return;
        }

        if (!SomeTasks[i]->Start(SomeTaskRoutine, nullptr)) {
            Panic("Can't start task");
            return;
        }
    }
}

void StopSomeTasks()
{
    for (size_t i = 0; i < Stdlib::ArraySize(SomeTasks); i++)
    {
        SomeTasks[i]->SetStopping();
        SomeTasks[i]->Wait();
        SomeTasks[i]->Put();
    }
}

void BpStartup(void* ctx)
{
    (void)ctx;

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

    //TODO: irq -> gsi remap by ACPI MADT
    Interrupt::Register(pit, acpi.GetGsiByIrq(0x2), 0x20);
    Interrupt::Register(kbd, acpi.GetGsiByIrq(0x1), 0x21);
    Interrupt::Register(serial, acpi.GetGsiByIrq(0x4), 0x24);

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

    Trace(0, "Interrupts enabled %u", (ulong)IsInterruptEnabled());

    Trace(0, "Before cpus start");

    if (!Parameters::GetInstance().IsSmpOff())
    {
        if (!cpus.StartAll())
        {
            Panic("Can't start all cpus");
            return;
        }
    }

    Trace(0, "Before preempt on");

    PreemptOn();

    Trace(0, "Preempt is now on");

    VgaTerm::GetInstance().Printf("IPI test...\n");

    ulong cpuMask = cpus.GetRunningCpus();
    for (ulong i = 0; i < 8 * sizeof(ulong); i++)
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
        Panic("Mulitasking test failed");
        return;
    }

    StartSomeTasks();

    VgaTerm::GetInstance().Printf("Idle looping...\n");

    if (!cmd.Start())
    {
        Panic("Can't start cmd");
        return;
    }

    for (;;)
    {
        cpu.Idle();
        if (cmd.ShouldShutdown())
        {
            Trace(0, "Shutdown");
            cmd.Stop();
            break;
        }
    }

    StopSomeTasks();

    Shutdown();
}

void Main2(Grub::MultiBootInfoHeader *MbInfo)
{
    do {

    ALLOC_CPU_STACK();

    Panicker::GetInstance();
    Watchdog::GetInstance();

    Trace(0, "Cpu rsp 0x%p rbp 0x%p", GetRsp(), GetRbp());

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
    auto& cmd = Cmd::GetInstance();
    auto& cpus = CpuTable::GetInstance();
    if (!kbd.RegisterObserver(cmd))
    {
        Panic("Can't register cmd in kbd");
        break;
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