#include "8042.h"
#include "vga.h"
#include "trace.h"
#include "new.h"
#include "panic.h"
#include "debug.h"
#include "spage_allocator.h"
#include "atomic.h"
#include "error.h"
#include "idt.h"
#include "grub.h"
#include "test.h"
#include "stdlib.h"
#include "memory_map.h"
#include "serial.h"
#include "pic.h"
#include "exception.h"
#include "pit.h"
#include "asm.h"
#include "acpi.h"
#include "cpu.h"
#include "cmd.h"
#include "lapic.h"
#include "ioapic.h"
#include "interrupt.h"

using namespace Kernel::Core;
using namespace Shared;

void ParseGrubInfo(Kernel::Grub::MultiBootInfoHeader *MbInfo)
{
    Trace(0, "MbInfo %p", MbInfo);

    Kernel::Grub::MultiBootTag * tag;
    for (tag = reinterpret_cast<Kernel::Grub::MultiBootTag*>(MbInfo + 1);
        tag->Type != Kernel::Grub::MultiBootTagTypeEnd;
        tag = reinterpret_cast<Kernel::Grub::MultiBootTag*>(MemAdd(tag, (tag->Size + 7) & ~7)))
    {
        Trace(0, "Tag %u Size %u", tag->Type, tag->Size);
        switch (tag->Type)
        {
        case Kernel::Grub::MultiBootTagTypeBootDev:
        {
            Kernel::Grub::MultiBootTagBootDev* bdev = reinterpret_cast<Kernel::Grub::MultiBootTagBootDev*>(tag);
            Trace(0, "Boot dev %u %u %u %u", (ulong)bdev->BiosDev, (ulong)bdev->Slice, (ulong)bdev->Part);
            break;
        }
        case Kernel::Grub::MultiBootTagTypeMmap:
        {
            Kernel::Grub::MultiBootTagMmap* mmap = reinterpret_cast<Kernel::Grub::MultiBootTagMmap*>(tag);
            Kernel::Grub::MultiBootMmapEntry* entry;

            for (entry = &mmap->Entry[0];
                 MemAdd(entry, mmap->EntrySize) <= MemAdd(mmap, mmap->Size);
                 entry = reinterpret_cast<Kernel::Grub::MultiBootMmapEntry*>(MemAdd(entry, mmap->EntrySize)))
            {
                Trace(0, "Mmap addr %p len %p type %u",
                    entry->Addr, entry->Len, (ulong)entry->Type);

                if (!MemoryMap::GetInstance().AddRegion(entry->Addr, entry->Len, entry->Type))
                    Panic("Can't add memory region");

            }
            break;
        }
        case Kernel::Grub::MultiBootTagTypeCmdline:
        {
            Kernel::Grub::MultiBootTagString* cmdLine = reinterpret_cast<Kernel::Grub::MultiBootTagString*>(tag);

            Trace(0, "Cmdline %s", cmdLine->String);
            break;
        }
        default:
        {
            break;
        }
        }
    }
}

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

extern "C" void ApMain()
{
    Lapic::Enable();

    auto& cpu = CpuTable::GetInstance().GetCurrentCpu();
    cpu.SetRunning();

    Trace(0, "Cpu %u started rflags 0x%p", cpu.GetIndex(), GetRflags());

    TraceCpuState(cpu.GetIndex());

    Idt::GetInstance().Save();

    InterruptEnable();

    for (;;)
    {
        cpu.Idle();
    }
}

extern "C" void Main(Kernel::Grub::MultiBootInfoHeader *MbInfo)
{
    Tracer::GetInstance().SetLevel(1);
    Trace(0, "Enter rflags 0x%p", GetRflags());

    VgaTerm::GetInstance().Printf("Hello!\n");

    ParseGrubInfo(MbInfo);

    auto& mmap = MemoryMap::GetInstance();

    ulong memStart, memEnd;

    //Kernel is loaded at 0x1000000 (see linke64.ld), so
    //assume 0x2000000 is high enough to use.
    //boot64.asm only setup paging for first 4GB
    //so do not overflow it.
    if (!mmap.FindRegion(0x2000000, 0x100000000, memStart, memEnd))
    {
        Panic("Can't get available memory region");
        return;
    }

    Trace(0, "Memory region 0x%p 0x%p", memStart, memEnd);
    SPageAllocator::GetInstance(memStart, memEnd);

    VgaTerm::GetInstance().Printf("Self test begin, please wait...\n");

    auto& acpi = Acpi::GetInstance();
    auto err = acpi.Parse();
    if (!err.Ok())
    {
        TraceError(err, "Can't parse ACPI");
        Panic("Can't parse ACPI");
        return;
    }

    err = Test();
    if (!err.Ok())
    {
        TraceError(err, "Test failed");
        Panic("Self test failed");
        return;
    }

    VgaTerm::GetInstance().Printf("Self test complete, error %u\n", (ulong)err.GetCode());

    auto& idt = Idt::GetInstance();
    auto& excTable = ExceptionTable::GetInstance();
    auto& pit = Pit::GetInstance();
    auto& kbd = IO8042::GetInstance();
    auto& serial = Serial::GetInstance();
    auto& cmd = Cmd::GetInstance();
    auto& pic = Pic::GetInstance();
    auto& ioApic = IoApic::GetInstance();
    auto& cpus = CpuTable::GetInstance();
    if (!kbd.RegisterObserver(cmd))
    {
        Panic("Can't register cmd in kbd");
        return;
    }

    pic.Remap();
    pic.Disable();

    Lapic::Enable();

    auto& cpu = cpus.GetCurrentCpu();
    if (!cpus.SetBspIndex(cpu.GetIndex()))
    {
        Panic("Can't set boot processor index");
        return;
    }

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
    InterruptEnable();

    if (!cpus.StartAll())
    {
        Panic("Can't start all cpus");
        return;
    }

    VgaTerm::GetInstance().Printf("IPI test...\n");

    ulong cpuMask = cpus.GetRunningCpus();
    for (ulong i = 0; i < 8 * sizeof(ulong); i++)
    {
        if (cpuMask & ((ulong)1 << i))
        {
            cpus.SendIPI(i);
        }
    }

    VgaTerm::GetInstance().Printf("Idle looping...\n");

    cmd.Start();
    for (;;)
    {
        cpu.Idle();
        cmd.Run();
        if (cmd.IsExit())
        {
            Trace(0, "Exit requested");
            break;
        }
    }

    Trace(0, "Exit");

    VgaTerm::GetInstance().Printf("Bye!\n");
}
