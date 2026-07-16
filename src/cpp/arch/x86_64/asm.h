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

void LoadTr(u16 selector);

void Outb(u16 port, u8 data);
u8 Inb(u16 port);

void Outw(u16 port, u16 data);
u16 Inw(u16 port);

void Out(u16 port, u32 data);
u32 In(u16 port);

u64 ReadMsr(u32 msr);
void WriteMsr(u32 msr, u64 value);

void SetRsp(ulong newValue);
void SetRbp(ulong newValue);

u64 ReadTsc();

u64 ReadTscp(u64 *cpuIndex);

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

static inline void Invlpg(unsigned long addr)
{
        asm volatile("invlpg (%0)" ::"r" (addr) : "memory");
}
