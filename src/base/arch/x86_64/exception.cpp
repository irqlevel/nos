#include "exception.h"
#include "pmu.h"

#include <kernel/profiler.h>
#include "idt.h"
#include "stdlib.h"
#include "asm.h"
#include <kernel/panic.h>
#include <kernel/trace.h>
#include <kernel/cpu.h>
#include <kernel/debug.h>

namespace kernel
{

ExceptionTable::ExceptionTable()
{
    trace(0, "ExcTable 0x%p", this);

    for (size_t i = 0; i < stdlib::array_size(handler_); i++)
    {
        handler_[i] = nullptr;
    }

    if (!set_handler(0x0, exc_divide_by_zero_stub))
        goto error;

    if (!set_handler(0x1, exc_debugger_stub))
        goto error;

    if (!set_handler(0x2, exc_nmi_stub))
        goto error;

    if (!set_handler(0x3, exc_breakpoint_stub))
        goto error;

    if (!set_handler(0x4, exc_overflow_stub))
        goto error;

    if (!set_handler(0x5, exc_bounds_stub))
        goto error;

    if (!set_handler(0x6, exc_invalid_opcode_stub))
        goto error;

    if (!set_handler(0x7, exc_coprocessor_not_available_stub))
        goto error;

    if (!set_handler(0x8, exc_double_fault_stub))
        goto error;

    if (!set_handler(0x9, exc_coprocessor_segment_overrun_stub))
        goto error;

    if (!set_handler(0xA, exc_invalid_task_state_segment_stub))
        goto error;

    if (!set_handler(0xB, exc_segment_not_present_stub))
        goto error;

    if (!set_handler(0xC, exc_stack_fault_stub))
        goto error;

    if (!set_handler(0xD, exc_general_protection_fault_stub))
        goto error;

    if (!set_handler(0xE, exc_page_fault_stub))
        goto error;

    if (!set_handler(0xF, exc_reserved_stub))
        goto error;

    if (!set_handler(0x10, exc_math_fault_stub))
        goto error;

    if (!set_handler(0x11, exc_alignment_check_stub))
        goto error;

    if (!set_handler(0x12, exc_machine_check_stub))
        goto error;

    if (!set_handler(0x13, exc_simd_fp_exception_stub))
        goto error;

    if (!set_handler(0x14, exc_virt_exception_stub))
        goto error;

    if (!set_handler(0x15, exc_control_protection_stub))
        goto error;

    return;

error:
    PANIC("EXC: Can't setup exception handler");
}

ExceptionTable::~ExceptionTable()
{
}

bool ExceptionTable::set_handler(size_t index, ExcHandler handler)
{
    if (index >= stdlib::array_size(handler_))
        return false;
    trace(0, "Set handler[%lu]=0x%p", index, handler);
    handler_[index] = handler;
    return true;
}

void ExceptionTable::register_exception_handlers()
{
    /* #DF is delivered on its own per-CPU IST stack (Gdt::setup_tss_self):
       it fires exactly when the current stack is unusable, and pushing the
       exception frame onto that bad stack would triple-fault instead of
       reaching the panic handler. */
    static const size_t DOUBLE_FAULT_VECTOR = 0x8;
    static const u8 DOUBLE_FAULT_IST = 1;

    auto& idt = Idt::get_instance();

    for (size_t i = 0; i < stdlib::array_size(handler_); i++)
    {
        u8 ist = (i == DOUBLE_FAULT_VECTOR) ? DOUBLE_FAULT_IST : 0;
        idt.set_descriptor(i, IdtDescriptor::encode(handler_[i], ist));
    }
}

void ExceptionTable::exc_divide_by_zero(Context* ctx)
{
    exc_divide_by_zero_counter_.inc();

    PANIC_CTX(ctx, false, "EXC: DivideByZero rip 0x%lX rsp 0x%lX",
        ctx->get_ret_rip(), ctx->get_orig_rsp());
}

void ExceptionTable::exc_debugger(Context* ctx)
{
    exc_debugger_counter_.inc();

    PANIC_CTX(ctx, false, "EXC: Debugger rip 0x%lX rsp 0x%lX",
        ctx->get_ret_rip(), ctx->get_orig_rsp());
}

void ExceptionTable::exc_nmi(Context* ctx)
{
    exc_nmi_counter_.inc();

    /* A panic on another CPU asking for this one's stack. It gets recorded
       and this CPU stops here: the panic is halting everything anyway, and
       panicking about the NMI itself would bury the report that sent it. */
    if (Panicker::get_instance().collect_remote_stack())
    {
        for (;;)
            cpu_halt();
    }

    /* A performance counter overflowing: a profiler sample, not a fault.
       Asked after the panic case and before the panic itself -- an NMI this
       kernel raised on purpose is the one kind it must return from. */
    if (Pmu::ack_overflow())
    {
        Profiler::get_instance().sample(ctx);
        return;
    }

    /* An AMD part delivering the tail of an overflow this CPU has already
       handled. Not a sample -- there is nothing new to record -- but not a
       fault to bring the machine down over either. */
    if (Pmu::absorb_spurious_nmi())
        return;

    PANIC_CTX(ctx, false, "EXC: NMI rip 0x%lX rsp 0x%lX",
        ctx->get_ret_rip(), ctx->get_orig_rsp());
}

void ExceptionTable::exc_breakpoint(Context* ctx)
{
    exc_breakpoint_counter_.inc();

    PANIC_CTX(ctx, false, "EXC: Breakpoint rip 0x%lX rsp 0x%lX",
        ctx->get_ret_rip(), ctx->get_orig_rsp());
}

void ExceptionTable::exc_overflow(Context* ctx)
{
    exc_overflow_counter_.inc();

    PANIC_CTX(ctx, false, "EXC: Overflow rip 0x%lX rsp 0x%lX",
        ctx->get_ret_rip(), ctx->get_orig_rsp());
}

void ExceptionTable::exc_bounds(Context* ctx)
{
    exc_bounds_counter_.inc();

    PANIC_CTX(ctx, false, "EXC: Bounds rip 0x%lX rsp 0x%lX",
        ctx->get_ret_rip(), ctx->get_orig_rsp());
}

void ExceptionTable::exc_invalid_opcode(Context* ctx)
{
    exc_invalid_opcode_counter_.inc();

    PANIC_CTX(ctx, false, "EXC: InvalidOpcode cpu %lu rip 0x%lX rsp 0x%lX",
        CpuTable::get_instance().get_current_cpu_id(),
        ctx->get_ret_rip(), ctx->get_orig_rsp());
}

void ExceptionTable::exc_coprocessor_not_available(Context* ctx)
{
    exc_coprocessor_not_available_counter_.inc();

    PANIC_CTX(ctx, false, "EXC: CoprocessorNotAvailable rip 0x%lX rsp 0x%lX",
        ctx->get_ret_rip(), ctx->get_orig_rsp());
}

void ExceptionTable::exc_double_fault(Context* ctx)
{
    exc_double_fault_counter_.inc();

    PANIC_CTX(ctx, true, "EXC: DoubleFault rip 0x%lX rsp 0x%lX err 0x%lX",
        ctx->get_ret_rip(true), ctx->get_orig_rsp(true), ctx->get_error_code());
}

void ExceptionTable::exc_coprocessor_segment_overrun(Context* ctx)
{
    exc_coprocessor_segment_overrun_counter_.inc();

    PANIC_CTX(ctx, false, "EXC: CoprocessorSegmentOverrun rip 0x%lX rsp 0x%lX",
        ctx->get_ret_rip(), ctx->get_orig_rsp());
}

void ExceptionTable::exc_invalid_task_state_segment(Context* ctx)
{
    exc_invalid_task_state_segment_counter_.inc();

    PANIC_CTX(ctx, true, "EXC: InvalidTSS rip 0x%lX rsp 0x%lX err 0x%lX",
        ctx->get_ret_rip(true), ctx->get_orig_rsp(true), ctx->get_error_code());
}

void ExceptionTable::exc_segment_not_present(Context* ctx)
{
    exc_segment_not_present_counter_.inc();

    PANIC_CTX(ctx, true, "EXC: SegmentNotPresent rip 0x%lX rsp 0x%lX err 0x%lX",
        ctx->get_ret_rip(true), ctx->get_orig_rsp(true), ctx->get_error_code());
}

void ExceptionTable::exc_stack_fault(Context* ctx)
{
    exc_stack_fault_counter_.inc();

    /* A read the panic report was not sure of: back into the report (see
       Panicker::recover_fault), and this one is not reported. The same in
       the next two. */
    Panicker::get_instance().recover_fault();

    PANIC_CTX(ctx, true, "EXC: StackFault rip 0x%lX rsp 0x%lX err 0x%lX",
        ctx->get_ret_rip(true), ctx->get_orig_rsp(true), ctx->get_error_code());
}

void ExceptionTable::exc_general_protection_fault(Context* ctx)
{
    exc_general_protection_fault_counter_.inc();

    Panicker::get_instance().recover_fault();

    PANIC_CTX(ctx, true, "EXC: GP cpu %lu rip 0x%lX rsp 0x%lX err 0x%lX",
        CpuTable::get_instance().get_current_cpu_id(),
        ctx->get_ret_rip(true), ctx->get_orig_rsp(true), ctx->get_error_code());
}

void ExceptionTable::exc_page_fault(Context* ctx)
{
    exc_page_fault_counter_.inc();

    Panicker::get_instance().recover_fault();

    PANIC_CTX(ctx, true, "EXC: PageFault cr2 0x%lX rip 0x%lX rsp 0x%lX err 0x%lX cr3 0x%lX",
        get_cr2(), ctx->get_ret_rip(true), ctx->get_orig_rsp(true),
        ctx->get_error_code(), get_cr3());
}

void ExceptionTable::exc_reserved(Context* ctx)
{
    exc_reserved_counter_.inc();

    PANIC_CTX(ctx, false, "EXC: Reserved rip 0x%lX rsp 0x%lX",
        ctx->get_ret_rip(), ctx->get_orig_rsp());
}

void ExceptionTable::exc_math_fault(Context* ctx)
{
    exc_math_fault_counter_.inc();

    PANIC_CTX(ctx, false, "EXC: MathFault rip 0x%lX rsp 0x%lX",
        ctx->get_ret_rip(), ctx->get_orig_rsp());
}

void ExceptionTable::exc_alignment_check(Context* ctx)
{
    exc_alignment_check_counter_.inc();

    PANIC_CTX(ctx, true, "EXC: AlignmentCheck rip 0x%lX rsp 0x%lX err 0x%lX",
        ctx->get_ret_rip(true), ctx->get_orig_rsp(true), ctx->get_error_code());
}

void ExceptionTable::exc_machine_check(Context* ctx)
{
    exc_machine_check_counter_.inc();

    PANIC_CTX(ctx, false, "EXC: MachineCheck rip 0x%lX rsp 0x%lX",
        ctx->get_ret_rip(), ctx->get_orig_rsp());
}

void ExceptionTable::exc_simd_fp_exception(Context* ctx)
{
    exc_simd_fp_exception_counter_.inc();

    PANIC_CTX(ctx, false, "EXC: SIMDFpException rip 0x%lX rsp 0x%lX",
        ctx->get_ret_rip(), ctx->get_orig_rsp());
}

void ExceptionTable::exc_virt_exception(Context* ctx)
{
    exc_virt_exception_counter_.inc();

    PANIC_CTX(ctx, false, "EXC: VirtException rip 0x%lX rsp 0x%lX",
        ctx->get_ret_rip(), ctx->get_orig_rsp());
}

void ExceptionTable::exc_control_protection(Context* ctx)
{
    exc_control_protection_counter_.inc();

    PANIC_CTX(ctx, true, "EXC: ControlProtection rip 0x%lX rsp 0x%lX err 0x%lX",
        ctx->get_ret_rip(true), ctx->get_orig_rsp(true), ctx->get_error_code());
}

extern "C" void exc_divide_by_zero(Context* ctx)
{
    auto& exc_table = ExceptionTable::get_instance();

    exc_table.exc_divide_by_zero(ctx);
}

extern "C" void exc_debugger(Context* ctx)
{
    auto& exc_table = ExceptionTable::get_instance();

    exc_table.exc_debugger(ctx);
}

extern "C" void exc_nmi(Context* ctx)
{
    auto& exc_table = ExceptionTable::get_instance();

    exc_table.exc_nmi(ctx);
}

extern "C" void exc_breakpoint(Context* ctx)
{
    auto& exc_table = ExceptionTable::get_instance();

    exc_table.exc_breakpoint(ctx);
}

extern "C" void exc_overflow(Context* ctx)
{
    auto& exc_table = ExceptionTable::get_instance();

    exc_table.exc_overflow(ctx);
}

extern "C" void exc_bounds(Context* ctx)
{
    auto& exc_table = ExceptionTable::get_instance();

    exc_table.exc_bounds(ctx);
}

extern "C" void exc_invalid_opcode(Context* ctx)
{
    auto& exc_table = ExceptionTable::get_instance();

    exc_table.exc_invalid_opcode(ctx);
}

extern "C" void exc_coprocessor_not_available(Context* ctx)
{
    auto& exc_table = ExceptionTable::get_instance();

    exc_table.exc_coprocessor_not_available(ctx);
}

extern "C" void exc_double_fault(Context* ctx)
{
    auto& exc_table = ExceptionTable::get_instance();

    exc_table.exc_double_fault(ctx);
}

extern "C" void exc_coprocessor_segment_overrun(Context* ctx)
{
    auto& exc_table = ExceptionTable::get_instance();

    exc_table.exc_coprocessor_segment_overrun(ctx);
}

extern "C" void exc_invalid_task_state_segment(Context* ctx)
{
    auto& exc_table = ExceptionTable::get_instance();

    exc_table.exc_invalid_task_state_segment(ctx);
}

extern "C" void exc_segment_not_present(Context* ctx)
{
    auto& exc_table = ExceptionTable::get_instance();

    exc_table.exc_segment_not_present(ctx);
}

extern "C" void exc_stack_fault(Context* ctx)
{
    auto& exc_table = ExceptionTable::get_instance();

    exc_table.exc_stack_fault(ctx);
}

extern "C" void exc_general_protection_fault(Context* ctx)
{
    auto& exc_table = ExceptionTable::get_instance();

    exc_table.exc_general_protection_fault(ctx);
}

extern "C" void exc_page_fault(Context* ctx)
{
    auto& exc_table = ExceptionTable::get_instance();

    exc_table.exc_page_fault(ctx);
}

extern "C" void exc_reserved(Context* ctx)
{
    auto& exc_table = ExceptionTable::get_instance();

    exc_table.exc_reserved(ctx);
}

extern "C" void exc_math_fault(Context* ctx)
{
    auto& exc_table = ExceptionTable::get_instance();

    exc_table.exc_math_fault(ctx);
}

extern "C" void exc_alignment_check(Context* ctx)
{
    auto& exc_table = ExceptionTable::get_instance();

    exc_table.exc_alignment_check(ctx);
}

extern "C" void exc_machine_check(Context* ctx)
{
    auto& exc_table = ExceptionTable::get_instance();

    exc_table.exc_machine_check(ctx);
}

extern "C" void exc_simd_fp_exception(Context* ctx)
{
    auto& exc_table = ExceptionTable::get_instance();

    exc_table.exc_simd_fp_exception(ctx);
}

extern "C" void exc_virt_exception(Context* ctx)
{
    auto& exc_table = ExceptionTable::get_instance();

    exc_table.exc_virt_exception(ctx);
}

extern "C" void exc_control_protection(Context* ctx)
{
    auto& exc_table = ExceptionTable::get_instance();

    exc_table.exc_control_protection(ctx);
}

}