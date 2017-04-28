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

using namespace Kernel;
using namespace Shared;

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

volatile bool PreemptOnWaiting = true;

void ApStartup(void *ctx)
{
    (void)ctx;

    auto& cpu = CpuTable::GetInstance().GetCurrentCpu();

    Trace(0, "Cpu %u running rflags 0x%p task 0x%p",
        cpu.GetIndex(), GetRflags(), Task::GetCurrentTask());

    TraceCpuState(cpu.GetIndex());

    Idt::GetInstance().Save();

    SetCr3(PageTable::GetInstance().GetRoot());

    InterruptEnable();

    cpu.SetRunning();

    while (PreemptOnWaiting)
    {
        cpu.Idle();
    }

    if (!TestMultiTasking())
    {
        Panic("Mulitasking test failed");
        return;
    }

    for (;;)
    {
        cpu.Idle();
    }
}

extern "C" void ApMain()
{
    Gdt::GetInstance().Save();
    Idt::GetInstance().Save();

    Lapic::Enable();

    auto& cpu = CpuTable::GetInstance().GetCurrentCpu();

    if (!cpu.Run(ApStartup, nullptr))
    {
        Trace(0, "Can't run cpu %u task", cpu.GetIndex());
        return;
    }
}

void Exit()
{
    PreemptDisable();

    VgaTerm::GetInstance().Printf("Going to exit!\n");
    Trace(0, "Exit begin");

    CpuTable::GetInstance().ExitAllExceptSelf();

    VgaTerm::GetInstance().Printf("Bye!\n");
    Trace(0, "Exit end");

    PreemptOff();

    __cxa_finalize(0);
    InterruptDisable();
    Hlt();
}

void BpStartup(void* ctx)
{
    (void)ctx;

    auto& idt = Idt::GetInstance();
    auto& excTable = ExceptionTable::GetInstance();
    auto& pit = Pit::GetInstance();
    auto& kbd = IO8042::GetInstance();
    auto& serial = Serial::GetInstance();
    auto& cmd = Cmd::GetInstance();
    auto& ioApic = IoApic::GetInstance();
    auto& cpus = CpuTable::GetInstance();
    auto& cpu = cpus.GetCurrentCpu();

    Trace(0, "Cpu %u running rflags 0x%p task 0x%p",
        cpu.GetIndex(), GetRflags(), Task::GetCurrentTask());

    TraceCpuState(cpu.GetIndex());

    ioApic.Enable();

    excTable.RegisterExceptionHandlers();

    //TODO: irq -> gsi remap by ACPI MADT
    Interrupt::Register(pit, 0x2, 0x20);
    Interrupt::Register(kbd, 0x1, 0x21);
    Interrupt::Register(serial, 0x4, 0x24);

    idt.SetDescriptor(CpuTable::IPIVector, IdtDescriptor::Encode(IPInterruptStub));

    idt.Save();
    pit.Setup();

    PageTable::GetInstance().UnmapNull();

    InterruptEnable();

    if (!cpus.StartAll())
    {
        Panic("Can't start all cpus");
        return;
    }

    PreemptOn();
    PreemptOnWaiting = false;

    VgaTerm::GetInstance().Printf("IPI test...\n");

    ulong cpuMask = cpus.GetRunningCpus();
    for (ulong i = 0; i < 8 * sizeof(ulong); i++)
    {
        if (cpuMask & ((ulong)1 << i))
        {
            cpus.SendIPI(i);
        }
    }

    VgaTerm::GetInstance().Printf("Task test...\n");

    if (!TestMultiTasking())
    {
        Panic("Mulitasking test failed");
        return;
    }

    VgaTerm::GetInstance().Printf("Idle looping...\n");

    if (!cmd.Start())
    {
        Panic("Can't start cmd");
        return;
    }

    for (;;)
    {
        cpu.Idle();
        if (cmd.IsExit())
        {
            Trace(0, "Exit requested");
            cmd.Stop();
            break;
        }
    }

    Exit();
}

extern "C" void Main(Grub::MultiBootInfoHeader *MbInfo)
{
    do {

    Gdt::GetInstance().Save();
    Idt::GetInstance().Save();

    if (!Dmesg::GetInstance().Setup())
    {
        Panic("Can't setup dmesg");
        return;
    }

    Tracer::GetInstance().SetLevel(1);

    VgaTerm::GetInstance().Printf("Hello!\n");

    Grub::ParseMultiBootInfo(MbInfo);

    auto& mmap = MemoryMap::GetInstance();
    Trace(0, "Enter kernel: start 0x%p end 0x%p",
        mmap.GetKernelStart(), mmap.GetKernelEnd());

    auto& pt = PageTable::GetInstance();
    if (!pt.Setup())
    {
        Panic("Can't setup paging");
        break;
    }

    Trace(0, "Paging root 0x%p old cr3 0x%p", pt.GetRoot(), GetCr3());

    SetCr3(pt.GetRoot());

    Trace(0, "Set new cr3 0x%p", GetCr3());

    ulong memStart, memEnd;
    if (mmap.GetKernelEnd() <= pt.PhysToVirt(MB))
    {
        Panic("Kernel end is lower than kernel space base");
        break;
    }

    //Since paging only setup for first 4GB use only first 4GB
    if (!mmap.FindRegion(pt.VirtToPhys(mmap.GetKernelEnd()), 4 * GB, memStart, memEnd))
    {
        Panic("Can't get available memory region");
        break;
    }

    Trace(0, "Memory region 0x%p 0x%p", memStart, memEnd);
    if (!PageAllocatorImpl::GetInstance().Setup(pt.PhysToVirt(memStart), pt.PhysToVirt(memEnd)))
    {
        Panic("Can't setup page allocator");
        break;
    }

    AllocatorImpl::GetInstance(PageAllocatorImpl::GetInstance());

    VgaTerm::GetInstance().Printf("Self test begin, please wait...\n");

    auto& acpi = Acpi::GetInstance();
    auto err = acpi.Parse();
    if (!err.Ok())
    {
        TraceError(err, "Can't parse ACPI");
        Panic("Can't parse ACPI");
        break;
    }

    Trace(0, "Before test");

    err = Test();
    if (!err.Ok())
    {
        TraceError(err, "Test failed");
        Panic("Self test failed");
        break;
    }

    Trace(0, "After test");
    VgaTerm::GetInstance().Printf("Self test complete, error %u\n", (ulong)err.GetCode());

    auto& kbd = IO8042::GetInstance();
    auto& cmd = Cmd::GetInstance();
    auto& pic = Pic::GetInstance();
    auto& cpus = CpuTable::GetInstance();
    if (!kbd.RegisterObserver(cmd))
    {
        Panic("Can't register cmd in kbd");
        break;
    }

    pic.Remap();
    pic.Disable();

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

    Exit();
}
