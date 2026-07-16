#include "gicv3.h"

#include <hal/context.h>
#include <kernel/panic.h>
#include <kernel/trace.h>
#include <kernel/interrupt.h>

/* EL1 exception entries called from vectors.S. The x86 twin is the
   ExceptionTable/IDT machinery in arch/x86_64. */

namespace Kernel
{

namespace
{

const char* EcName(ulong ec)
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

extern "C" void ArmSyncEntry(Context* ctx)
{
    ulong ec = (ctx->Esr >> 26) & 0x3F;
    ulong iss = ctx->Esr & 0x1FFFFFF;

    PanicCtx(ctx, false,
        "sync exception: %s (esr 0x%p ec 0x%p iss 0x%p) far 0x%p elr 0x%p",
        EcName(ec), ctx->Esr, ec, iss, ctx->Far, ctx->Elr);
}

extern "C" void ArmUnexpectedEntry(Context* ctx, ulong index)
{
    PanicCtx(ctx, false,
        "unexpected exception vector %u (esr 0x%p far 0x%p elr 0x%p)",
        index, ctx->Esr, ctx->Far, ctx->Elr);
}

extern "C" char Arm64Vectors[];

void SetupVectors()
{
    asm volatile("msr vbar_el1, %0; isb" :: "r"(&Arm64Vectors[0]));
}

}
