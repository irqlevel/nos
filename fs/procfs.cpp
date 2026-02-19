#include "procfs.h"

#include <lib/stdlib.h>
#include <kernel/parameters.h>
#include <kernel/interrupt.h>
#include <kernel/trace.h>

namespace Kernel
{

ProcFs::ProcFs()
    : InterruptsNode(nullptr)
{
}

ProcFs::~ProcFs()
{
}

const char* ProcFs::GetName()
{
    return "procfs";
}

bool ProcFs::Mount()
{
    VNode* root = GetRoot();

    /* /proc/version */
    {
        char buf[128];
        Stdlib::SnPrintf(buf, sizeof(buf), "nos %s", KERNEL_VERSION);
        VNode* node = CreateFile(root, "version");
        if (node != nullptr)
            Write(node, buf, Stdlib::StrLen(buf));
    }

    /* /proc/cmdline */
    {
        const char* cmdline = Parameters::GetInstance().GetCmdline();
        VNode* node = CreateFile(root, "cmdline");
        if (node != nullptr)
            Write(node, cmdline, Stdlib::StrLen(cmdline));
    }

    /* /proc/interrupts — dynamic, refreshed on each read */
    InterruptsNode = CreateFile(root, "interrupts");
    if (InterruptsNode != nullptr)
        RefreshInterrupts();

    return true;
}

void ProcFs::RefreshInterrupts()
{
    static const ulong BufSize = 512;
    char buf[BufSize];
    ulong pos = 0;

    for (u8 i = 0; i < InterruptStats::Count; i++)
    {
        InterruptSource src = static_cast<InterruptSource>(i);
        long count = InterruptStats::Get(src);
        const char* name = InterruptStats::GetName(src);
        int n = Stdlib::SnPrintf(buf + pos, BufSize - pos,
                                 "%-12s %10ld\n", name, count);
        if (n > 1)
            pos = pos + (ulong)(n - 1);
    }

    RamFs::Write(InterruptsNode, buf, pos);
}

bool ProcFs::Read(VNode* file, void* buf, ulong len, ulong offset)
{
    if (file == InterruptsNode)
    {
        RefreshInterrupts();
        Stdlib::MemSet(buf, 0, len);
    }

    return RamFs::Read(file, buf, len, offset);
}

}
