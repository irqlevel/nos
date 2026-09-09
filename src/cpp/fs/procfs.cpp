#include "procfs.h"
#include "version_gen.h"

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
        Stdlib::SnPrintf(buf, sizeof(buf), "nos %s (%s)", KERNEL_VERSION,
            KERNEL_GIT_REV);
        VNode* node = CreateFile(root, "version");
        if (node != nullptr)
            Write(node, buf, Stdlib::StrLen(buf), 0);
    }

    /* /proc/cmdline */
    {
        const char* cmdline = Parameters::GetInstance().GetCmdline();
        VNode* node = CreateFile(root, "cmdline");
        if (node != nullptr)
            Write(node, cmdline, Stdlib::StrLen(cmdline), 0);
    }

    /* /proc/interrupts -- dynamic, refreshed on each lookup */
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
        if (n > 0)
            pos = pos + (ulong)n;
    }

    RamFs::Truncate(InterruptsNode, 0);
    RamFs::Write(InterruptsNode, buf, pos, 0);
}

/* The Vfs looks a file up before it reads it or reports its size, so
   refreshing here keeps both consistent with each other. */
VNode* ProcFs::Lookup(VNode* dir, const char* name)
{
    VNode* node = RamFs::Lookup(dir, name);
    if (node != nullptr && node == InterruptsNode)
        RefreshInterrupts();
    return node;
}

}
