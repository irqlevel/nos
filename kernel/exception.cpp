#include "exception.h"
#include "idt.h"
#include "stdlib.h"
#include "asm.h"
#include "panic.h"
#include "trace.h"
#include "cpu.h"

namespace Kernel
{

ExceptionTable::ExceptionTable()
{
    for (size_t i = 0; i < Shared::ArraySize(Handler); i++)
    {
        Handler[i] = nullptr;
    }

    if (!SetHandler(0x0, ExcDivideByZeroStub))
        goto error;

    if (!SetHandler(0x1, ExcDebuggerStub))
        goto error;

    if (!SetHandler(0x2, ExcNMIStub))
        goto error;

    if (!SetHandler(0x3, ExcBreakpointStub))
        goto error;

    if (!SetHandler(0x4, ExcOverflowStub))
        goto error;

    if (!SetHandler(0x5, ExcBoundsStub))
        goto error;

    if (!SetHandler(0x6, ExcInvalidOpcodeStub))
        goto error;

    if (!SetHandler(0x7, ExcCoprocessorNotAvailableStub))
        goto error;

    if (!SetHandler(0x8, ExcDoubleFaultStub))
        goto error;

    if (!SetHandler(0x9, ExcCoprocessorSegmentOverrunStub))
        goto error;

    if (!SetHandler(0xA, ExcInvalidTaskStateSegmentStub))
        goto error;

    if (!SetHandler(0xB, ExcSegmentNotPresentStub))
        goto error;

    if (!SetHandler(0xC, ExcStackFaultStub))
        goto error;

    if (!SetHandler(0xD, ExcGeneralProtectionFaultStub))
        goto error;

    if (!SetHandler(0xE, ExcPageFaultStub))
        goto error;

    if (!SetHandler(0xF, ExcReservedStub))
        goto error;

    if (!SetHandler(0x10, ExcMathFaultStub))
        goto error;

    if (!SetHandler(0x11, ExcAlignmentCheckStub))
        goto error;

    if (!SetHandler(0x12, ExcMachineCheckStub))
        goto error;

    if (!SetHandler(0x13, ExcSIMDFpExceptionStub))
        goto error;

    if (!SetHandler(0x14, ExcVirtExceptionStub))
        goto error;

    if (!SetHandler(0x15, ExcControlProtectionStub))
        goto error;

    return;

error:
    Panic("EXC: Can't setup exception handler");
}

ExceptionTable::~ExceptionTable()
{
}

bool ExceptionTable::SetHandler(size_t index, ExcHandler handler)
{
    if (index >= Shared::ArraySize(Handler))
        return false;
    Handler[index] = handler;
    return true;
}

void ExceptionTable::RegisterExceptionHandlers()
{
    auto& idt = Idt::GetInstance();

    for (size_t i = 0; i < Shared::ArraySize(Handler); i++)
    {
        idt.SetDescriptor(i, IdtDescriptor::Encode(Handler[i]));
    }
}

void ExceptionTable::ExcDivideByZero(Context* ctx)
{
    (void)ctx;

    ExcDivideByZeroCounter.Inc();

    Panic("EXC: DivideByZero");
}

void ExceptionTable::ExcDebugger(Context* ctx)
{
    (void)ctx;

    ExcDebuggerCounter.Inc();

    Panic("EXC: Debugger");
}

void ExceptionTable::ExcNMI(Context* ctx)
{
    (void)ctx;

    ExcNMICounter.Inc();

    Panic("EXC: NMI");
}

void ExceptionTable::ExcBreakpoint(Context* ctx)
{
    (void)ctx;

    ExcBreakpointCounter.Inc();

    Panic("EXC: Breakpoint");
}

void ExceptionTable::ExcOverflow(Context* ctx)
{
    (void)ctx;

    ExcOverflowCounter.Inc();

    Panic("EXC: Overflow");
}

void ExceptionTable::ExcBounds(Context* ctx)
{
    (void)ctx;

    ExcBoundsCounter.Inc();

    Panic("EXC: Bounds");
}

void ExceptionTable::ExcInvalidOpcode(Context* ctx)
{
    (void)ctx;

    ExcInvalidOpcodeCounter.Inc();

    Panic("EXC: InvalidOpcode cpu %u rip 0x%p rsp 0x%p",
        CpuTable::GetInstance().GetCurrentCpuId(), ctx->GetRetRip(), ctx->Rsp);
}

void ExceptionTable::ExcCoprocessorNotAvailable(Context* ctx)
{
    (void)ctx;

    ExcCoprocessorNotAvailableCounter.Inc();

    Panic("EXC: CoprocessorNotAvailable");
}

void ExceptionTable::ExcDoubleFault(Context* ctx)
{
    (void)ctx;

    ExcDoubleFaultCounter.Inc();

    Panic("EXC: DoubleFault");
}

void ExceptionTable::ExcCoprocessorSegmentOverrun(Context* ctx)
{
    (void)ctx;

    ExcCoprocessorSegmentOverrunCounter.Inc();

    Panic("EXC: CoprocessorSegmentOverrun");
}

void ExceptionTable::ExcInvalidTaskStateSegment(Context* ctx)
{
    (void)ctx;

    ExcInvalidTaskStateSegmentCounter.Inc();

    Panic("EXC: InvalidTaskStateSegment");
}

void ExceptionTable::ExcSegmentNotPresent(Context* ctx)
{
    (void)ctx;

    ExcSegmentNotPresentCounter.Inc();

    Panic("EXC: SegmentNotPresent");
}

void ExceptionTable::ExcStackFault(Context* ctx)
{
    (void)ctx;

    ExcStackFaultCounter.Inc();

    Panic("EXC: StackFault");
}

void ExceptionTable::ExcGeneralProtectionFault(Context* ctx)
{
    (void)ctx;

    ExcGeneralProtectionFaultCounter.Inc();

    Panic("EXC: GP cpu %u rip 0x%p rsp 0x%p",
        CpuTable::GetInstance().GetCurrentCpuId(), ctx->GetRetRip(), ctx->Rsp);
}

void ExceptionTable::ExcPageFault(Context* ctx)
{
    (void)ctx;

    ExcPageFaultCounter.Inc();

    Panic("EXC: PageFault cpu %u rip 0x%p rsp 0x%p cr2 0x%p cr3 0x%p",
        CpuTable::GetInstance().GetCurrentCpuId(), ctx->GetRetRip(), ctx->Rsp,
        GetCr2(), GetCr3());
}

void ExceptionTable::ExcReserved(Context* ctx)
{
    (void)ctx;

    ExcReservedCounter.Inc();

    Panic("EXC: Reserved");
}

void ExceptionTable::ExcMathFault(Context* ctx)
{
    (void)ctx;

    ExcMathFaultCounter.Inc();

    Panic("EXC: MathFault");
}

void ExceptionTable::ExcAlignmentCheck(Context* ctx)
{
    (void)ctx;

    ExcAlignmentCheckCounter.Inc();

    Panic("EXC: AlignmentCheck");
}

void ExceptionTable::ExcMachineCheck(Context* ctx)
{
    (void)ctx;

    ExcMachineCheckCounter.Inc();

    Panic("EXC: MachineCheck");
}

void ExceptionTable::ExcSIMDFpException(Context* ctx)
{
    (void)ctx;

    ExcSIMDFpExceptionCounter.Inc();

    Panic("EXC: SIMDFpException");
}

void ExceptionTable::ExcVirtException(Context* ctx)
{
    (void)ctx;

    ExcVirtExceptionCounter.Inc();

    Panic("EXC: VirtException");
}

void ExceptionTable::ExcControlProtection(Context* ctx)
{
    (void)ctx;

    ExcControlProtectionCounter.Inc();

    Panic("EXC: ControlProtection");
}

extern "C" void ExcDivideByZero(Context* ctx)
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcDivideByZero(ctx);
}

extern "C" void ExcDebugger(Context* ctx)
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcDebugger(ctx);
}

extern "C" void ExcNMI(Context* ctx)
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcNMI(ctx);
}

extern "C" void ExcBreakpoint(Context* ctx)
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcBreakpoint(ctx);
}

extern "C" void ExcOverflow(Context* ctx)
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcOverflow(ctx);
}

extern "C" void ExcBounds(Context* ctx)
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcBounds(ctx);
}

extern "C" void ExcInvalidOpcode(Context* ctx)
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcInvalidOpcode(ctx);
}

extern "C" void ExcCoprocessorNotAvailable(Context* ctx)
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcCoprocessorNotAvailable(ctx);
}

extern "C" void ExcDoubleFault(Context* ctx)
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcDoubleFault(ctx);
}

extern "C" void ExcCoprocessorSegmentOverrun(Context* ctx)
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcCoprocessorSegmentOverrun(ctx);
}

extern "C" void ExcInvalidTaskStateSegment(Context* ctx)
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcInvalidTaskStateSegment(ctx);
}

extern "C" void ExcSegmentNotPresent(Context* ctx)
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcSegmentNotPresent(ctx);
}

extern "C" void ExcStackFault(Context* ctx)
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcStackFault(ctx);
}

extern "C" void ExcGeneralProtectionFault(Context* ctx)
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcGeneralProtectionFault(ctx);
}

extern "C" void ExcPageFault(Context* ctx)
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcPageFault(ctx);
}

extern "C" void ExcReserved(Context* ctx)
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcReserved(ctx);
}

extern "C" void ExcMathFault(Context* ctx)
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcMathFault(ctx);
}

extern "C" void ExcAlignmentCheck(Context* ctx)
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcAlignmentCheck(ctx);
}

extern "C" void ExcMachineCheck(Context* ctx)
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcMachineCheck(ctx);
}

extern "C" void ExcSIMDFpException(Context* ctx)
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcSIMDFpException(ctx);
}

extern "C" void ExcVirtException(Context* ctx)
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcVirtException(ctx);
}

extern "C" void ExcControlProtection(Context* ctx)
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcControlProtection(ctx);
}

}