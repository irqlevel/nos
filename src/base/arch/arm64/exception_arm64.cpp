#include "gicv3.h"

#include <hal/context.h>
#include <kernel/panic.h>
#include <kernel/klog.h>
#include <kernel/interrupt.h>
#include <kernel/cpu.h>
#include <kernel/task.h>

/* EL1 exception entries called from vectors.S. The x86 twin is the
   ExceptionTable/IDT machinery in arch/x86_64. */

/* What a CPU takes a fault of its stack on (vectors.S's vec_stack_fault):
   16 KiB each, as x86's double fault has -- a report, the netconsole
   sending it, went 5688 bytes deep there. Its top is the address after
   it. */
static const unsigned long ARM64_OVERFLOW_STACK_SHIFT = 14;
extern "C"
{
alignas(16) char arm64_overflow_stacks[kernel::MAX_CPUS][1UL << ARM64_OVERFLOW_STACK_SHIFT];
}

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

/* A data abort within a page of the stack pointer, taken on this CPU's
   overflow stack (vectors.S): the stack ran out -- a task's, into the page
   under it -- or was never there. Not one to return from. */
extern "C" void arm_stack_fault_entry(Context* ctx)
{
    /* A read the panic report was not sure of, next to its own stack
       pointer: back into the report, as arm_sync_entry's */
    Panicker::get_instance().recover_fault();

    PANIC_CTX(ctx, false,
        "sync exception: data abort at 0x%lX, within a page of the stack pointer 0x%lX (elr 0x%lX esr 0x%lX)%s",
        ctx->far, ctx->sp, ctx->elr, ctx->esr,
        Task::in_stack_guard(ctx->far) ? " -- the page under a stack: the stack ran out" :
            ": the stack ran out, or is not mapped");
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
