#pragma once

#include <include/types.h>
#include <hal/cpu.h>
#include <hal/barrier.h>
#include <hal/irq_stubs.h>
#include <arch/x86_64/context.h>

// x86_64-only CPU primitives (defined in arch/x86_64/asm.asm). The portable
// contract lives in hal/cpu.h; nothing outside arch/x86_64 (plus the
// documented exemptions: kernel/main.cpp, kernel/cmd.cpp,
// kernel/irq_balance.cpp and the x86-only drivers) may include this header.

#ifdef __cplusplus
extern "C"
{
#endif

ulong get_cr0(void);
ulong get_cr1(void);
ulong get_cr2(void);
ulong get_cr3(void);
ulong get_cr4(void);

void set_cr3(ulong addr);

ulong get_rsp(void);
ulong get_rbp(void);
ulong get_rip(void);

ulong get_rflags(void);
void set_rflags(ulong rflags);

ulong get_cs(void);
ulong get_ds(void);
ulong get_ss(void);
ulong get_es(void);
ulong get_fs(void);
ulong get_gs(void);

void store_idt(void *dest);
void load_idt(void *src);

void store_gdt(void *dest);
void load_gdt(void *src);

void load_tr(u16 selector);

void outb(u16 port, u8 data);
u8 inb(u16 port);

void outw(u16 port, u16 data);
u16 inw(u16 port);

void outl(u16 port, u32 data);
u32 inl(u16 port);

u64 read_msr(u32 msr);
void write_msr(u32 msr, u64 value);

void set_rsp(ulong new_value);
void set_rbp(ulong new_value);

u64 read_tsc();

void exc_divide_by_zero_stub();
void exc_debugger_stub();
void exc_nmi_stub();
void exc_breakpoint_stub();
void exc_overflow_stub();
void exc_bounds_stub();
void exc_invalid_opcode_stub();
void exc_coprocessor_not_available_stub();
void exc_double_fault_stub();
void exc_coprocessor_segment_overrun_stub();
void exc_invalid_task_state_segment_stub();
void exc_segment_not_present_stub();
void exc_stack_fault_stub();
void exc_general_protection_fault_stub();
void exc_page_fault_stub();
void exc_reserved_stub();
void exc_math_fault_stub();
void exc_alignment_check_stub();
void exc_machine_check_stub();
void exc_simd_fp_exception_stub();
void exc_virt_exception_stub();
void exc_control_protection_stub();

#ifdef __cplusplus
}
#endif

static inline void invlpg(unsigned long addr)
{
        asm volatile("invlpg (%0)" ::"r" (addr) : "memory");
}
