#include "gdt.h"
#include "asm.h"
#include <kernel/trace.h>
#include <kernel/panic.h>
#include <kernel/cpu.h>

#include <lib/stdlib.h>

namespace Kernel
{

static_assert(Gdt::TssMaxCpus == MaxCpus, "TssMaxCpus must equal MaxCpus");

Gdt::Gdt()
{
    Trace(0, "Gdt 0x%p", this);

    Entry[1].SetValue(((u64)1<<43) | ((u64)1<<44) | ((u64)1<<47) | ((u64)1<<53));
}

void Gdt::SetupTssSelf(ulong cpuIndex)
{
    /* System-descriptor encoding (SDM Vol. 3, 16-byte 64-bit TSS form) */
    static const u64 TssTypeAvail64 = 0x9; /* available 64-bit TSS */
    static const u64 DescPresent = 1;

    if (BugOn(cpuIndex >= TssMaxCpus))
        return;

    Tss* tss = &TssArray[cpuIndex];
    Stdlib::MemSet(tss, 0, sizeof(*tss));
    tss->Ist1 = reinterpret_cast<u64>(&DfStack[cpuIndex][DfStackSize]);
    tss->IoMapBase = sizeof(Tss); /* no I/O permission bitmap */

    u64 base = reinterpret_cast<u64>(tss);
    u64 limit = sizeof(Tss) - 1;

    u64 low = (limit & 0xFFFF)
        | ((base & 0xFFFFFF) << 16)
        | (TssTypeAvail64 << 40)
        | (DescPresent << 47)
        | (((limit >> 16) & 0xF) << 48)
        | (((base >> 24) & 0xFF) << 56);

    Entry[TssEntryBase + 2 * cpuIndex].SetValue(low);
    Entry[TssEntryBase + 2 * cpuIndex + 1].SetValue(base >> 32);

    LoadTr((u16)((TssEntryBase + 2 * cpuIndex) * sizeof(GdtDescriptor)));
}

void Gdt::Save()
{
    TableDesc desc = {
        .Limit = sizeof(Entry),
        .Base = reinterpret_cast<u64>(&Entry[0]),
    };

    LoadGdt(&desc);

    Base = desc.Base;
    Limit = desc.Limit;
}

Gdt::~Gdt()
{
}

}