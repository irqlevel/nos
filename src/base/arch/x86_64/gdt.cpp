#include "gdt.h"
#include "asm.h"
#include <kernel/trace.h>
#include <kernel/panic.h>
#include <kernel/cpu.h>

#include <lib/stdlib.h>

namespace kernel
{

static_assert(Gdt::TSS_MAX_CPUS == MAX_CPUS, "TSS_MAX_CPUS must equal MAX_CPUS");

Gdt::Gdt()
{
    trace(0, "Gdt 0x%p", this);

    entry_[1].set_value(((u64)1<<43) | ((u64)1<<44) | ((u64)1<<47) | ((u64)1<<53));
}

void Gdt::setup_tss_self(ulong cpu_index)
{
    /* System-descriptor encoding (SDM Vol. 3, 16-byte 64-bit TSS form) */
    static const u64 TSS_TYPE_AVAIL64 = 0x9; /* available 64-bit TSS */
    static const u64 DESC_PRESENT = 1;

    if (BUG_ON(cpu_index >= TSS_MAX_CPUS))
        return;

    Tss* tss = &tss_array_[cpu_index];
    stdlib::mem_set(tss, 0, sizeof(*tss));
    tss->ist1 = reinterpret_cast<u64>(&df_stack_[cpu_index][DF_STACK_SIZE]);
    tss->io_map_base = sizeof(Tss); /* no I/O permission bitmap */

    u64 base = reinterpret_cast<u64>(tss);
    u64 limit = sizeof(Tss) - 1;

    u64 low = (limit & 0xFFFF)
        | ((base & 0xFFFFFF) << 16)
        | (TSS_TYPE_AVAIL64 << 40)
        | (DESC_PRESENT << 47)
        | (((limit >> 16) & 0xF) << 48)
        | (((base >> 24) & 0xFF) << 56);

    entry_[TSS_ENTRY_BASE + 2 * cpu_index].set_value(low);
    entry_[TSS_ENTRY_BASE + 2 * cpu_index + 1].set_value(base >> 32);

    load_tr((u16)((TSS_ENTRY_BASE + 2 * cpu_index) * sizeof(GdtDescriptor)));
}

void Gdt::save()
{
    TableDesc desc = {
        .limit = sizeof(entry_),
        .base = reinterpret_cast<u64>(&entry_[0]),
    };

    load_gdt(&desc);

    base_ = desc.base;
    limit_ = desc.limit;
}

Gdt::~Gdt()
{
}

}