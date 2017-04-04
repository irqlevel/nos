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

void DumpCpuState()
{
    Trace(0, "Cpu cr0 0x%p cr2 0x%p cr3 0x%p cr4 0x%p",
        GetCr0(), GetCr2(), GetCr3(), GetCr4());

    Trace(0, "Cpu rflags 0x%p rsp 0x%p rip 0x%p",
        GetRflags(), GetRsp(), GetRip());

    Trace(0, "Cpu ss 0x%p cs 0x%p ds 0x%p gs 0x%p fs 0x%p es 0x%p",
        (ulong)GetSs(), (ulong)GetCs(), (ulong)GetDs(),
        (ulong)GetGs(), (ulong)GetFs(), (ulong)GetEs());
}

extern "C" void kernel_main(Kernel::Grub::MultiBootInfoHeader *MbInfo)
{
    Tracer::GetInstance().SetLevel(1);
    Trace(0, "Enter");

    VgaTerm::GetInstance().Printf("Hello!\n");

    DumpCpuState();

    ParseGrubInfo(MbInfo);

    auto& mmap = MemoryMap::GetInstance();

    ulong memStart, memEnd;

    //Kernel is loaded at 0x1000000 (see linke64.ld), so
    //assume 0x2000000 is high enough to use
    if (!mmap.FindRegion(0x2000000, memStart, memEnd))
    {
        Panic("Can't get available memory region");
        return;
    }

    //boot64.asm only setup paging for first 0x40000000 bytes(1GB)
    //so do not overflow it
    if (memEnd > 0x40000000)
        memEnd = 0x40000000;

    Trace(0, "Memory region 0x%p 0x%p", memStart, memEnd);
    SPageAllocator::GetInstance(memStart, memEnd);

    VgaTerm::GetInstance().Printf("Self test begin, please wait...\n");

    auto& acpi = Acpi::GetInstance();
    if (!acpi.Parse())
    {
        Trace(0, "Can't parse ACPI");
    }

    auto err = Test();
    TraceError(err);

    VgaTerm::GetInstance().Printf("Self test complete, error %u\n", (ulong)err.GetCode());

    auto& idt = Idt::GetInstance();

    auto& excTable = ExceptionTable::GetInstance();
    auto& pit = Pit::GetInstance();
    auto& kbd = IO8042::GetInstance();
    auto& serial = Serial::GetInstance();

    Pic::GetInstance().Remap();
    excTable.RegisterInterrupts();
    pit.RegisterInterrupt(0x20);
    kbd.RegisterInterrupt(0x21);
    serial.RegisterInterrupt(0x24);
    idt.Save();
    pit.Setup();
    Enable();

    VgaTerm::GetInstance().Printf("Idle looping...\n");

    for (;;)
    {
        Hlt();
    }

    Trace(0, "Exit");

    VgaTerm::GetInstance().Printf("Bye!\n");
}
