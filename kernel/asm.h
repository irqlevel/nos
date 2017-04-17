#pragma once

#include <include/types.h>

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

void SetRsp(ulong newValue);

void SwitchContext(ulong nextRsp, ulong* currRsp);

void AtomicInc(volatile long *pvalue);
void AtomicDec(volatile long *pvalue);
long AtomicRead(volatile long *pvalue);
void AtomicWrite(volatile long *pvalue, long newValue);
long AtomicReadAndDec(volatile long *pvalue);
long AtomicCmpxchg(volatile long *pvalue, long exchange, long comparand);

long AtomicTestAndSetBit(volatile long *pvalue, ulong bit);
long AtomicTestBit(volatile long *pvalue, ulong bit);

void IO8042InterruptStub();
void SerialInterruptStub();
void PitInterruptStub();
void IPInterruptStub();

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

#define Barrier() __asm__ __volatile__("": : :"memory")

namespace Kernel
{

struct Context final
{
    ulong R15;
    ulong R14;
    ulong R13;
    ulong R12;
    ulong R11;
    ulong R10;
    ulong R9;
    ulong R8;
    ulong Rsi;
    ulong Rdi;
    ulong Rdx;
    ulong Rcx;
    ulong Rbx;
    ulong Rax;
    ulong Rbp;
    ulong Rflags;
    ulong Rsp;

    ulong GetRetRip()
    {
        return *((ulong *)Rsp);
    }
private:
    Context(const Context& other) = delete;
    Context(Context&& other) = delete;
    Context& operator=(const Context& other) = delete;
    Context& operator=(Context&& other) = delete;
};

}