#include "exception.h"
#include "idt.h"
#include "stdlib.h"
#include "asm.h"
#include "panic.h"
#include "trace.h"
#include "cpu.h"
#include "debug.h"

namespace Kernel
{

ExceptionTable::ExceptionTable()
{
    Trace(0, "ExcTable 0x%p", this);

    for (size_t i = 0; i < Stdlib::ArraySize(Handler); i++)
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
    if (index >= Stdlib::ArraySize(Handler))
        return false;
    Trace(0, "Set handler[%u]=0x%p", index, handler);
    Handler[index] = handler;
    return true;
}

void ExceptionTable::RegisterExceptionHandlers()
{
    auto& idt = Idt::GetInstance();

    for (size_t i = 0; i < Stdlib::ArraySize(Handler); i++)
    {
        idt.SetDescriptor(i, IdtDescriptor::Encode(Handler[i]));
    }
}

void ExceptionTable::ExcDivideByZero(Context* ctx)
{
    ExcDivideByZeroCounter.Inc();

    PanicCtx(ctx, false, "EXC: DivideByZero rip 0x%p rsp 0x%p",
        ctx->GetRetRip(), ctx->GetOrigRsp());
}

void ExceptionTable::ExcDebugger(Context* ctx)
{
    ExcDebuggerCounter.Inc();

    PanicCtx(ctx, false, "EXC: Debugger rip 0x%p rsp 0x%p",
        ctx->GetRetRip(), ctx->GetOrigRsp());
}

void ExceptionTable::ExcNMI(Context* ctx)
{
    ExcNMICounter.Inc();

    PanicCtx(ctx, false, "EXC: NMI rip 0x%p rsp 0x%p",
        ctx->GetRetRip(), ctx->GetOrigRsp());
}

void ExceptionTable::ExcBreakpoint(Context* ctx)
{
    ExcBreakpointCounter.Inc();

    PanicCtx(ctx, false, "EXC: Breakpoint rip 0x%p rsp 0x%p",
        ctx->GetRetRip(), ctx->GetOrigRsp());
}

void ExceptionTable::ExcOverflow(Context* ctx)
{
    ExcOverflowCounter.Inc();

    PanicCtx(ctx, false, "EXC: Overflow rip 0x%p rsp 0x%p",
        ctx->GetRetRip(), ctx->GetOrigRsp());
}

void ExceptionTable::ExcBounds(Context* ctx)
{
    ExcBoundsCounter.Inc();

    PanicCtx(ctx, false, "EXC: Bounds rip 0x%p rsp 0x%p",
        ctx->GetRetRip(), ctx->GetOrigRsp());
}

void ExceptionTable::ExcInvalidOpcode(Context* ctx)
{
    ExcInvalidOpcodeCounter.Inc();

    PanicCtx(ctx, false, "EXC: InvalidOpcode cpu %u rip 0x%p rsp 0x%p",
        CpuTable::GetInstance().GetCurrentCpuId(),
        ctx->GetRetRip(), ctx->GetOrigRsp());
}

void ExceptionTable::ExcCoprocessorNotAvailable(Context* ctx)
{
    ExcCoprocessorNotAvailableCounter.Inc();

    PanicCtx(ctx, false, "EXC: CoprocessorNotAvailable rip 0x%p rsp 0x%p",
        ctx->GetRetRip(), ctx->GetOrigRsp());
}

void ExceptionTable::ExcDoubleFault(Context* ctx)
{
    ExcDoubleFaultCounter.Inc();

    PanicCtx(ctx, true, "EXC: DoubleFault rip 0x%p rsp 0x%p err 0x%p",
        ctx->GetRetRip(true), ctx->GetOrigRsp(true), ctx->GetErrorCode());
}

void ExceptionTable::ExcCoprocessorSegmentOverrun(Context* ctx)
{
    ExcCoprocessorSegmentOverrunCounter.Inc();

    PanicCtx(ctx, false, "EXC: CoprocessorSegmentOverrun rip 0x%p rsp 0x%p",
        ctx->GetRetRip(), ctx->GetOrigRsp());
}

void ExceptionTable::ExcInvalidTaskStateSegment(Context* ctx)
{
    ExcInvalidTaskStateSegmentCounter.Inc();

    PanicCtx(ctx, true, "EXC: InvalidTSS rip 0x%p rsp 0x%p err 0x%p",
        ctx->GetRetRip(true), ctx->GetOrigRsp(true), ctx->GetErrorCode());
}

void ExceptionTable::ExcSegmentNotPresent(Context* ctx)
{
    ExcSegmentNotPresentCounter.Inc();

    PanicCtx(ctx, true, "EXC: SegmentNotPresent rip 0x%p rsp 0x%p err 0x%p",
        ctx->GetRetRip(true), ctx->GetOrigRsp(true), ctx->GetErrorCode());
}

void ExceptionTable::ExcStackFault(Context* ctx)
{
    ExcStackFaultCounter.Inc();

    PanicCtx(ctx, true, "EXC: StackFault rip 0x%p rsp 0x%p err 0x%p",
        ctx->GetRetRip(true), ctx->GetOrigRsp(true), ctx->GetErrorCode());
}

void ExceptionTable::ExcGeneralProtectionFault(Context* ctx)
{
    ExcGeneralProtectionFaultCounter.Inc();

    PanicCtx(ctx, true, "EXC: GP cpu %u rip 0x%p rsp 0x%p err 0x%p",
        CpuTable::GetInstance().GetCurrentCpuId(),
        ctx->GetRetRip(true), ctx->GetOrigRsp(true), ctx->GetErrorCode());
}

void ExceptionTable::ExcPageFault(Context* ctx)
{
    ExcPageFaultCounter.Inc();

    PanicCtx(ctx, true, "EXC: PageFault cr2 0x%p rip 0x%p rsp 0x%p err 0x%p cr3 0x%p",
        GetCr2(), ctx->GetRetRip(true), ctx->GetOrigRsp(true),
        ctx->GetErrorCode(), GetCr3());
}

void ExceptionTable::ExcReserved(Context* ctx)
{
    ExcReservedCounter.Inc();

    PanicCtx(ctx, false, "EXC: Reserved rip 0x%p rsp 0x%p",
        ctx->GetRetRip(), ctx->GetOrigRsp());
}

void ExceptionTable::ExcMathFault(Context* ctx)
{
    ExcMathFaultCounter.Inc();

    PanicCtx(ctx, false, "EXC: MathFault rip 0x%p rsp 0x%p",
        ctx->GetRetRip(), ctx->GetOrigRsp());
}

void ExceptionTable::ExcAlignmentCheck(Context* ctx)
{
    ExcAlignmentCheckCounter.Inc();

    PanicCtx(ctx, true, "EXC: AlignmentCheck rip 0x%p rsp 0x%p err 0x%p",
        ctx->GetRetRip(true), ctx->GetOrigRsp(true), ctx->GetErrorCode());
}

void ExceptionTable::ExcMachineCheck(Context* ctx)
{
    ExcMachineCheckCounter.Inc();

    PanicCtx(ctx, false, "EXC: MachineCheck rip 0x%p rsp 0x%p",
        ctx->GetRetRip(), ctx->GetOrigRsp());
}

void ExceptionTable::ExcSIMDFpException(Context* ctx)
{
    ExcSIMDFpExceptionCounter.Inc();

    PanicCtx(ctx, false, "EXC: SIMDFpException rip 0x%p rsp 0x%p",
        ctx->GetRetRip(), ctx->GetOrigRsp());
}

void ExceptionTable::ExcVirtException(Context* ctx)
{
    ExcVirtExceptionCounter.Inc();

    PanicCtx(ctx, false, "EXC: VirtException rip 0x%p rsp 0x%p",
        ctx->GetRetRip(), ctx->GetOrigRsp());
}

void ExceptionTable::ExcControlProtection(Context* ctx)
{
    ExcControlProtectionCounter.Inc();

    PanicCtx(ctx, true, "EXC: ControlProtection rip 0x%p rsp 0x%p err 0x%p",
        ctx->GetRetRip(true), ctx->GetOrigRsp(true), ctx->GetErrorCode());
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