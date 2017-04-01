#ifndef __HELPERS_32_H__
#define __HELPERS_32_H__

#include "types.h"

#ifdef __cplusplus
extern "C"
{
#endif

ulong get_cr0_32(void);
ulong get_cr1_32(void);
ulong get_cr2_32(void);
ulong get_cr3_32(void);
ulong get_cr4_32(void);

ulong get_esp_32(void);
ulong get_eip_32(void);

ulong get_eflags_32(void);

void pause_32(void);

ulong get_cs_32(void);
ulong get_ds_32(void);
ulong get_ss_32(void);
ulong get_es_32(void);
ulong get_fs_32(void);
ulong get_gs_32(void);

void get_idt_32(void *result);
void put_idt_32(void *result);
void get_gdt_32(void *result);

ulong check_cpuid_support_32(void);
ulong check_long_mode_support_32(void);

void spin_lock_lock_32(ulong *lock);
void spin_lock_unlock_32(ulong *lock);

void outb(u16, u8);
u8 inb(u16);

void enable(void);
void disable(void);
void hlt(void);

void IO8042InterruptStub();
void SerialInterruptStub();
void PitInterruptStub();

void DummyInterruptStub();

void ExcDivideByZeroStub();
void ExcDebuggerStub();
void ExcNMIStub();
void ExcBreakpointStub();
void ExcOverflowStub();
void ExcBoundsStub();
void ExcInvalidOpcodeStub();
void ExcCoprocessorNotAvailableStub();
void ExcDoubleFaultStub();
void ExcCoprocessorSegmentOverrunStub();
void ExcInvalidTaskStateSegmentStub();
void ExcSegmentNotPresentStub();
void ExcStackFaultStub();
void ExcGeneralProtectionFaultStub();
void ExcPageFaultStub();
void ExcReservedStub();
void ExcMathFaultStub();
void ExcAlignmentCheckStub();
void ExcMachineCheckStub();
void ExcSIMDFpExceptionStub();
void ExcVirtExceptionStub();
void ExcControlProtectionStub();

#ifdef __cplusplus
}
#endif

#endif
