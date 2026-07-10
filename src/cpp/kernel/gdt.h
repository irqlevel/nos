#pragma once

#include "gdt_descriptor.h"
#include <lib/stdlib.h>

namespace Kernel
{

/* 64-bit TSS. Only the IST stack-switch fields are used (no ring
   transitions, no I/O permission bitmap). */
struct Tss
{
    u32 Reserved0;
    u64 Rsp0;
    u64 Rsp1;
    u64 Rsp2;
    u64 Reserved1;
    u64 Ist1;
    u64 Ist2;
    u64 Ist3;
    u64 Ist4;
    u64 Ist5;
    u64 Ist6;
    u64 Ist7;
    u64 Reserved2;
    u16 Reserved3;
    u16 IoMapBase;
} __attribute__((packed));

static_assert(sizeof(Tss) == 104, "Invalid Tss size");

class Gdt final
{
public:
    static Gdt& GetInstance()
    {
        static Gdt Instance;
        return Instance;
    }

    void Save();

    /* Write this CPU's TSS descriptor and load TR with it. The TSS's IST1
       points at a dedicated per-CPU stack used by the double-fault gate, so
       #DF is delivered on a known-good stack even when RSP is corrupt --
       without it the CPU pushes the #DF frame onto the bad stack and
       triple-faults, losing all panic diagnostics. Requires Save() to have
       run on this CPU. Safe to call again after a GDT reload: the
       descriptor is rewritten first, which clears the busy bit ltr sets. */
    void SetupTssSelf(ulong cpuIndex);

    /* Must equal MaxCpus (static_assert in gdt.cpp); gdt.h cannot include
       cpu.h without an include cycle. */
    static const ulong TssMaxCpus = 8;

private:
    Gdt();
    ~Gdt();

    struct TableDesc {
	    u16 Limit;
	    u64 Base;
    } __attribute((packed));

    static const ulong TssEntryBase = 2; /* null, code, then TSS pairs */
    static const ulong DfStackSize = 4096;

    u64 Base;
    u16 Limit;

    /* null + code + one 16-byte (two-slot) TSS descriptor per CPU */
    GdtDescriptor Entry[TssEntryBase + 2 * TssMaxCpus];

    Tss TssArray[TssMaxCpus];
    alignas(16) u8 DfStack[TssMaxCpus][DfStackSize];
};

}
