#include "8042.h"
#include "vga.h"
#include "trace.h"
#include "new.h"
#include "panic.h"
#include "debug.h"
#include "unique_ptr.h"
#include "sallocator.h"
#include "spage_allocator.h"
#include "atomic.h"
#include "shared_ptr.h"
#include "btree.h"
#include "error.h"
#include "vector.h"
#include "gdt.h"
#include "idt.h"
#include "idt_descriptor.h"
#include "cpu_state.h"
#include "grub.h"
#include "test.h"
#include "stdlib.h"
#include "memory_map.h"
#include "serial.h"
#include "pic.h"
#include "exception.h"
#include "pit.h"

using namespace Kernel::Core;
using namespace Shared;

extern "C" void kernel_main(Kernel::Grub::MultiBootInfo *MbInfo)
{
    Tracer::GetInstance().SetLevel(1);
    Trace(0, "Enter");

    VgaTerm::GetInstance().Printf("Hello!\n");

    auto& mmap = MemoryMap::GetInstance(MbInfo);

    ulong freeMemStart, freeMemEnd;
    if (!mmap.GetFreeRegion(0x2000000, freeMemStart, freeMemEnd))
    {
        Panic("Can't get free memory region");
        return;
    }

    Trace(0, "Memory region %p %p", freeMemStart, freeMemEnd);
    SPageAllocator::GetInstance(freeMemStart, freeMemEnd);

    auto err = Test();
    TraceError(err);

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
    enable();

    for (;;)
    {
        hlt();
    }

    Trace(0, "Exit");
    VgaTerm::GetInstance().Printf("Bye!\n");
}
