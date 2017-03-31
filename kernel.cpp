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

using namespace Kernel::Core;
using namespace Shared;

extern "C" void kernel_main(Kernel::Grub::MultiBootInfo *MbInfo)
{
    Tracer::GetInstance().SetLevel(1);

    Trace(0, "Enter");

    auto& mmap = MemoryMap::GetInstance(MbInfo);

    ulong freeMemStart, freeMemEnd;
    if (!mmap.GetFreeRegion(0x2000000, freeMemStart, freeMemEnd))
    {
        Panic("Can't get free memory region");
        return;
    }

    Trace(0, "Free memory region %p %p", freeMemStart, freeMemEnd);
    SPageAllocator::GetInstance(freeMemStart, freeMemEnd);

    auto err = Test();
    TraceError(err);

    Gdt gdt;

    gdt.Load();

    Trace(0, "Gdt base 0x%p limit 0x%p", (ulong)gdt.GetBase(), (ulong)gdt.GetLimit());

    for (u16 selector = 0; selector < gdt.GetLimit(); selector+= 8)
    {
        GdtDescriptor desc = gdt.LoadDescriptor(selector);
        if (desc.GetValue() == 0)
            continue;

        Trace(0, "Gdt[0x%p] desc 0x%p limit 0x%p access 0x%p flag 0x%p",
            (ulong)selector, (ulong)desc.GetBase(), (ulong)desc.GetLimit(),
            (ulong)desc.GetAccess(), (ulong)desc.GetFlag());
       
    }

    static IdtDescriptor idt[256];
    Idt idtr;
    idtr.Save(idt, sizeof(idt));

    CpuState cpu;
    cpu.Load();

    Trace(0, "Cpu cr0 0x%p cr1 0x%p cr2 0x%p cr3 0x%p cr4 0x%p",
        cpu.GetCr0(), cpu.GetCr1(), cpu.GetCr2(), cpu.GetCr3(),
        cpu.GetCr4());

    Trace(0, "Cpu eflags 0x%p sp 0x%p",
        cpu.GetEflags(), cpu.GetEsp());

    Trace(0, "Cpu ss 0x%p cs 0x%p ds 0x%p gs 0x%p fs 0x%p es 0x%p",
        (ulong)cpu.GetSs(), (ulong)cpu.GetCs(), (ulong)cpu.GetDs(),
        (ulong)cpu.GetGs(), (ulong)cpu.GetFs(), (ulong)cpu.GetEs());

    auto& kbd = IO8042::GetInstance();

    // FIXME: reconfigure 8259 to remap IRQs
    kbd.Register(&idt[9]);
    outb(0x21,0xfd);
    outb(0xa1,0xff);
    enable();

    auto& term = VgaTerm::GetInstance();
    u8 mod = 0;
    while(1) {
        static char map[0x80] = "__1234567890-=_" "\tqwertyuiop[]\n" "_asdfghjkl;'`" "_\\zxcvbnm,./_" "*_ _";
        u8 c = kbd.get();
        if (c == 0x2a || c == 0x36) mod = 0x20;
        else if (c == 0xaa || c == 0xb6) mod = 0x00;
        else if (c & 0x80) continue;
        else term.Printf("%c", map[(int) c] ^ mod);
    }

    Trace(0, "Exit");
}
