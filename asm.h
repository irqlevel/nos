#pragma once

#include "types.h"

#ifdef __cplusplus
extern "C"
{
#endif

ulong GetCr0(void);
ulong GetCr1(void);
ulong GetCr2(void);
ulong GetCr3(void);
ulong GetCr4(void);

ulong GetRsp(void);
ulong GetRip(void);

ulong GetRflags(void);
void SetRflags(ulong rflags);

void Pause(void);

ulong GetCs(void);
ulong GetDs(void);
ulong GetSs(void);
ulong GetEs(void);
ulong GetFs(void);
ulong GetGs(void);

void GetIdt(void *result);
void PutIdt(void *result);
void GetGdt(void *result);


void SpinLockLock(ulong *lock);
void SpinLockUnlock(ulong *lock);

void Outb(u16 port, u8 data);
u8 Inb(u16 port);

u64 ReadMsr(u32 msr);
void WriteMsr(u32 msr, u64 value);

void InterruptEnable(void);
void InterruptDisable(void);
void Hlt(void);

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

#define Barrier() __asm__ __volatile__("": : :"memory")

#endif
