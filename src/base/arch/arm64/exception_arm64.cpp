#include "gicv3.h"

#include <hal/context.h>
#include <kernel/panic.h>
#include <kernel/trace.h>
#include <kernel/interrupt.h>

/* EL1 exception entries called from vectors.S. The x86 twin is the
   ExceptionTable/IDT machinery in arch/x86_64. */

namespace kernel
{

namespace
{

const char* ec_name(ulong ec)
{
    switch (ec)
    {
    case 0x00: return "unknown/udf";
    case 0x15: return "svc";
    case 0x20: return "instr abort (lower EL)";
    case 0x21: return "instr abort";
    case 0x22: return "pc alignment";
    case 0x24: return "data abort (lower EL)";
    case 0x25: return "data abort";
    case 0x26: return "sp alignment";
    case 0x30: return "breakpoint (lower EL)";
    case 0x31: return "breakpoint";
    case 0x3C: return "brk";
    default: return "?";
    }
}

}

extern "C" void arm_sync_entry(Context* ctx)
{
    /* A read the panic report was not sure of: back into the report (see
       Panicker::recover_fault), and this one is not reported */
    Panicker::get_instance().recover_fault();

    ulong ec = (ctx->esr >> 26) & 0x3F;
    ulong iss = ctx->esr & 0x1FFFFFF;

    PANIC_CTX(ctx, false,
        "sync exception: %s (esr 0x%lX ec 0x%lX iss 0x%lX) far 0x%lX elr 0x%lX",
        ec_name(ec), ctx->esr, ec, iss, ctx->far, ctx->elr);
}

extern "C" void arm_unexpected_entry(Context* ctx, ulong index)
{
    PANIC_CTX(ctx, false,
        "unexpected exception vector %lu (esr 0x%lX far 0x%lX elr 0x%lX)",
        index, ctx->esr, ctx->far, ctx->elr);
}

extern "C" char arm64_vectors[];

void setup_vectors()
{
    asm volatile("msr vbar_el1, %0; isb" :: "r"(&arm64_vectors[0]));
}

}
