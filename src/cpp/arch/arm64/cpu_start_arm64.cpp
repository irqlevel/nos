#include <kernel/cpu.h>
#include <kernel/trace.h>

namespace Kernel
{

/* arm64 AP bring-up (PSCI CPU_ON) arrives with milestone M4; until then
   the kernel runs single-CPU and StartAll trivially succeeds (the x86
   twin is arch/x86_64/cpu_start.cpp). */
bool CpuTable::StartAll()
{
    Trace(0, "StartAll: single CPU until PSCI bring-up");
    return true;
}

}
