#include "pl011.h"

#include <lib/stdlib.h>
#include <mm/memory_map.h>

/* M1 "hello serial" entry: prove Image-header boot, EL1, the MMU switch to
   the higher half and C++ execution. Grows into the Main2-equivalent as the
   arm64 bring-up proceeds (plans/02-hal-arm64.md). */

namespace
{

const ulong Pl011PhysBase = 0x09000000;

}

extern "C" void MainArm64(void* dtb)
{
    using namespace Kernel;

    Pl011::EarlyInit(Mm::MemoryMap::KernelSpaceBase + Pl011PhysBase);
    Pl011::PrintString("nos arm64: hello from EL1 (higher half)\n");

    char msg[64];
    Stdlib::SnPrintf(msg, sizeof(msg), "nos arm64: dtb at 0x%p\n", dtb);
    Pl011::PrintString(msg);

    for (;;)
    {
        asm volatile("wfi");
    }
}
