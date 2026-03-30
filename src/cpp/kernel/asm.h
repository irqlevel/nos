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

void SetCr3(ulong addr);

ulong GetRsp(void);
ulong GetRbp(void);
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

void StoreIdt(void *dest);
void LoadIdt(void *src);

void StoreGdt(void *dest);
void LoadGdt(void *src);

void SpinLockLock(ulong *lock);
void SpinLockUnlock(ulong *lock);

void Outb(u16 port, u8 data);
u8 Inb(u16 port);

void Outw(u16 port, u16 data);
u16 Inw(u16 port);

void Out(u16 port, u32 data);
u32 In(u16 port);

u64 ReadMsr(u32 msr);
void WriteMsr(u32 msr, u64 value);

void InterruptEnable(void);
void InterruptDisable(void);
void Hlt(void);

void SetRsp(ulong newValue);
void SetRbp(ulong newValue);

u64 ReadTsc();

u64 ReadTscp(u64 *cpuIndex);

void SwitchContext(ulong nextRsp, ulong* currRsp, void (*callback)(void* ctx), void* ctx);

void AtomicInc(volatile long *pvalue);
void AtomicDec(volatile long *pvalue);
void AtomicAdd(volatile long *pvalue, long delta);
long AtomicRead(volatile long *pvalue);
void AtomicWrite(volatile long *pvalue, long newValue);
long AtomicReadAndDec(volatile long *pvalue);
long AtomicReadAndInc(volatile long *pvalue);

long AtomicCmpxchg(volatile long *pvalue, long exchange, long comparand);

long AtomicTestAndSetBit(volatile long *pvalue, ulong bit);
long AtomicTestAndClearBit(volatile long *pvalue, ulong bit);
long AtomicTestBit(volatile long *pvalue, ulong bit);

void IO8042InterruptStub();
void SerialInterruptStub();
void PitInterruptStub();
void HpetInterruptStub();
void IPInterruptStub();
void VirtioBlkInterruptStub();
void VirtioNetInterruptStub();
void VirtioScsiInterruptStub();
void SharedInterruptStub();

void DummyInterruptStub();

void RustInterruptStub0();
void RustInterruptStub1();
void RustInterruptStub2();
void RustInterruptStub3();
void RustInterruptStub4();
void RustInterruptStub5();
void RustInterruptStub6();
void RustInterruptStub7();

void RustMsixStub0();
void RustMsixStub1();
void RustMsixStub2();
void RustMsixStub3();
void RustMsixStub4();
void RustMsixStub5();
void RustMsixStub6();
void RustMsixStub7();
void RustMsixStub8();
void RustMsixStub9();
void RustMsixStub10();
void RustMsixStub11();
void RustMsixStub12();
void RustMsixStub13();
void RustMsixStub14();
void RustMsixStub15();
void RustMsixStub16();
void RustMsixStub17();
void RustMsixStub18();
void RustMsixStub19();
void RustMsixStub20();
void RustMsixStub21();
void RustMsixStub22();
void RustMsixStub23();
void RustMsixStub24();
void RustMsixStub25();
void RustMsixStub26();
void RustMsixStub27();
void RustMsixStub28();
void RustMsixStub29();
void RustMsixStub30();
void RustMsixStub31();

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

long SetJmp(void *ctx);
void LongJmp(void *ctx, long result);

#ifdef __cplusplus
}
#endif

#define Barrier() __asm__ __volatile__("": : :"memory")

static inline void Invlpg(unsigned long addr)
{
        asm volatile("invlpg (%0)" ::"r" (addr) : "memory");
}

static inline void Pause(ulong count)
{
    for (ulong i = 0; i < count; i++)
        Pause();
}

namespace Kernel
{

struct JmpContext final
{
    ulong Rbx;
    ulong Rsp;
    ulong Rbp;
    ulong R12;
    ulong R13;
    ulong R14;
    ulong R15;
    ulong RetAddr;
};

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

    ulong GetRetRip(bool hasErrorCode = false)
    {
        if (hasErrorCode)
            return *((ulong *)(Rsp + sizeof(ulong)));
        return *((ulong *)Rsp);
    }

    ulong GetErrorCode()
    {
        return *((ulong *)Rsp);
    }

    ulong GetOrigRsp(bool hasErrorCode = false)
    {
        if (hasErrorCode)
            return *((ulong *)(Rsp + 4 * sizeof(ulong)));
        return *((ulong *)(Rsp + 3 * sizeof(ulong)));
    }
private:
    Context(const Context& other) = delete;
    Context(Context&& other) = delete;
    Context& operator=(const Context& other) = delete;
    Context& operator=(Context&& other) = delete;
};

static inline bool IsInterruptEnabled()
{
    return (GetRflags() & 0x200) ? true : false;
}

}