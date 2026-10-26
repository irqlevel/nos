#pragma once

#include <kernel/atomic.h>
#include "asm.h"

#include <include/types.h>

namespace kernel
{

class ExceptionTable final
{

public:
    static ExceptionTable& get_instance()
    {
        static ExceptionTable instance;

        return instance;
    }

    void register_exception_handlers();

    void exc_divide_by_zero(Context* ctx);
    void exc_debugger(Context* ctx);
    void exc_nmi(Context* ctx);
    void exc_breakpoint(Context* ctx);
    void exc_overflow(Context* ctx);
    void exc_bounds(Context* ctx);
    void exc_invalid_opcode(Context* ctx);
    void exc_coprocessor_not_available(Context* ctx);
    void exc_double_fault(Context* ctx);
    void exc_coprocessor_segment_overrun(Context* ctx);
    void exc_invalid_task_state_segment(Context* ctx);
    void exc_segment_not_present(Context* ctx);
    void exc_stack_fault(Context* ctx);
    void exc_general_protection_fault(Context* ctx);
    void exc_page_fault(Context* ctx);
    void exc_reserved(Context* ctx);
    void exc_math_fault(Context* ctx);
    void exc_alignment_check(Context* ctx);
    void exc_machine_check(Context* ctx);
    void exc_simd_fp_exception(Context* ctx);
    void exc_virt_exception(Context* ctx);
    void exc_control_protection(Context* ctx);

private:
    ExceptionTable();
    ~ExceptionTable();

    ExceptionTable(const ExceptionTable& other) = delete;
    ExceptionTable(ExceptionTable& other) = delete;

    ExceptionTable& operator=(const ExceptionTable& other) = delete;
    ExceptionTable& operator=(ExceptionTable& other) = delete; 

    using ExcHandler = void (*)();

    bool set_handler(size_t index, ExcHandler handler);

    ExcHandler handler_[0x16];

    Atomic exc_divide_by_zero_counter_;
    Atomic exc_debugger_counter_;
    Atomic exc_nmi_counter_;
    Atomic exc_breakpoint_counter_;
    Atomic exc_overflow_counter_;
    Atomic exc_bounds_counter_;
    Atomic exc_invalid_opcode_counter_;
    Atomic exc_coprocessor_not_available_counter_;
    Atomic exc_double_fault_counter_;
    Atomic exc_coprocessor_segment_overrun_counter_;
    Atomic exc_invalid_task_state_segment_counter_;
    Atomic exc_segment_not_present_counter_;
    Atomic exc_stack_fault_counter_;
    Atomic exc_general_protection_fault_counter_;
    Atomic exc_page_fault_counter_;
    Atomic exc_reserved_counter_;
    Atomic exc_math_fault_counter_;
    Atomic exc_alignment_check_counter_;
    Atomic exc_machine_check_counter_;
    Atomic exc_simd_fp_exception_counter_;
    Atomic exc_virt_exception_counter_;
    Atomic exc_control_protection_counter_;
};

}