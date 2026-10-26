#pragma once

#include "gdt_descriptor.h"
#include <lib/stdlib.h>

namespace kernel
{

/* 64-bit TSS. Only the IST stack-switch fields are used (no ring
   transitions, no I/O permission bitmap). */
struct Tss
{
    u32 reserved0;
    u64 rsp0;
    u64 rsp1;
    u64 rsp2;
    u64 reserved1;
    u64 ist1;
    u64 ist2;
    u64 ist3;
    u64 ist4;
    u64 ist5;
    u64 ist6;
    u64 ist7;
    u64 reserved2;
    u16 reserved3;
    u16 io_map_base;
} __attribute__((packed));

static_assert(sizeof(Tss) == 104, "Invalid Tss size");

class Gdt final
{
public:
    static Gdt& get_instance()
    {
        static Gdt instance;
        return instance;
    }

    void save();

    /* Write this CPU's TSS descriptor and load TR with it. The TSS's IST1
       points at a dedicated per-CPU stack used by the double-fault gate, so
       #DF is delivered on a known-good stack even when RSP is corrupt --
       without it the CPU pushes the #DF frame onto the bad stack and
       triple-faults, losing all panic diagnostics. Requires Save() to have
       run on this CPU. Safe to call again after a GDT reload: the
       descriptor is rewritten first, which clears the busy bit ltr sets. */
    void setup_tss_self(ulong cpu_index);

    /* Must equal MaxCpus (static_assert in gdt.cpp); gdt.h cannot include
       cpu.h without an include cycle. */
    static const ulong TSS_MAX_CPUS = 64;

private:
    Gdt();
    ~Gdt();

    struct TableDesc {
	    u16 limit;
	    u64 base;
    } __attribute((packed));

    static const ulong TSS_ENTRY_BASE = 2; /* null, code, then TSS pairs */
    /* What a double fault's report runs on, all of it: one, the netconsole
       sending it, went 5688 bytes deep here (StackProbe, on a build for the
       purpose) -- past the page this was, into the next CPU's. */
    static const ulong DF_STACK_SIZE = 4 * consts::PAGE_SIZE;

    u64 base_;
    u16 limit_;

    /* null + code + one 16-byte (two-slot) TSS descriptor per CPU */
    GdtDescriptor entry_[TSS_ENTRY_BASE + 2 * TSS_MAX_CPUS];

    Tss tss_array_[TSS_MAX_CPUS];
    alignas(16) u8 df_stack_[TSS_MAX_CPUS][DF_STACK_SIZE];
};

}
