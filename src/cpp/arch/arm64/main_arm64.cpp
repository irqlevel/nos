#include "pl011.h"
#include "board.h"

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
}

extern "C" void MainArm64(void* dtb)
{
    using namespace Kernel;

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

    Trace(0, "After test");

    Trace(0, "boot: m2 complete");

    for (;;)
    {
        asm volatile("wfi");
    }
}
