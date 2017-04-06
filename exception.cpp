#include "exception.h"
#include "idt.h"
#include "stdlib.h"
#include "asm.h"
#include "panic.h"
#include "trace.h"

namespace Kernel
{

namespace Core
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
    Panic("Can't setup exception handler");
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

void ExceptionTable::ExcDivideByZero()
{
    ExcDivideByZeroCounter.Inc();

    Trace(ExcLL, "EXC: DivideByZero");
}

void ExceptionTable::ExcDebugger()
{
    ExcDebuggerCounter.Inc();

    Trace(ExcLL, "EXC: Debugger");
}

void ExceptionTable::ExcNMI()
{
    ExcNMICounter.Inc();

    Trace(ExcLL, "EXC: NMI");
}

void ExceptionTable::ExcBreakpoint()
{
    ExcBreakpointCounter.Inc();

    Trace(ExcLL, "EXC: Breakpoint");
}

void ExceptionTable::ExcOverflow()
{
    ExcOverflowCounter.Inc();

    Trace(ExcLL, "EXC: Overflow");
}

void ExceptionTable::ExcBounds()
{
    ExcBoundsCounter.Inc();

    Trace(ExcLL, "EXC: Bounds");
}

void ExceptionTable::ExcInvalidOpcode()
{
    ExcInvalidOpcodeCounter.Inc();

    Trace(ExcLL, "EXC: InvalidOpCode");
}

void ExceptionTable::ExcCoprocessorNotAvailable()
{
    ExcCoprocessorNotAvailableCounter.Inc();

    Trace(ExcLL, "EXC: CoprocessorNotAvailable");
}

void ExceptionTable::ExcDoubleFault()
{
    ExcDoubleFaultCounter.Inc();

    Trace(ExcLL, "EXC: DoubleFault");
}

void ExceptionTable::ExcCoprocessorSegmentOverrun()
{
    ExcCoprocessorSegmentOverrunCounter.Inc();

    Trace(ExcLL, "EXC: CoprocessorSegmentOverrun");
}

void ExceptionTable::ExcInvalidTaskStateSegment()
{
    ExcInvalidTaskStateSegmentCounter.Inc();

    Trace(ExcLL, "EXC: InvalidTaskStateSegment");
}

void ExceptionTable::ExcSegmentNotPresent()
{
    ExcSegmentNotPresentCounter.Inc();

    Trace(ExcLL, "EXC: SegmentNotPresent");
}

void ExceptionTable::ExcStackFault()
{
    ExcStackFaultCounter.Inc();

    Trace(ExcLL, "EXC: StackFault");
}

void ExceptionTable::ExcGeneralProtectionFault()
{
    ExcGeneralProtectionFaultCounter.Inc();

    Trace(ExcLL, "EXC: GeneralProtectionFault");
}

void ExceptionTable::ExcPageFault()
{
    ExcPageFaultCounter.Inc();

    Trace(ExcLL, "EXC: PageFault");
}

void ExceptionTable::ExcReserved()
{
    ExcReservedCounter.Inc();

    Trace(ExcLL, "EXC: Reserved");
}

void ExceptionTable::ExcMathFault()
{
    ExcMathFaultCounter.Inc();

    Trace(ExcLL, "EXC: MathFault");
}

void ExceptionTable::ExcAlignmentCheck()
{
    ExcAlignmentCheckCounter.Inc();

    Trace(ExcLL, "EXC: AlignmentCheck");
}

void ExceptionTable::ExcMachineCheck()
{
    ExcMachineCheckCounter.Inc();

    Trace(ExcLL, "EXC: MachineCheck");
}

void ExceptionTable::ExcSIMDFpException()
{
    ExcSIMDFpExceptionCounter.Inc();

    Trace(ExcLL, "EXC: SIMDFpException");
}

void ExceptionTable::ExcVirtException()
{
    ExcVirtExceptionCounter.Inc();

    Trace(ExcLL, "EXC: VirtException");
}

void ExceptionTable::ExcControlProtection()
{
    ExcControlProtectionCounter.Inc();

    Trace(ExcLL, "EXC: ControlProtection");
}

extern "C" void ExcDivideByZero()
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcDivideByZero();
}

extern "C" void ExcDebugger()
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcDebugger();
}

extern "C" void ExcNMI()
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcNMI();
}

extern "C" void ExcBreakpoint()
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcBreakpoint();
}

extern "C" void ExcOverflow()
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcOverflow();
}

extern "C" void ExcBounds()
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcBounds();
}

extern "C" void ExcInvalidOpcode()
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcInvalidOpcode();
}

extern "C" void ExcCoprocessorNotAvailable()
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcCoprocessorNotAvailable();
}

extern "C" void ExcDoubleFault()
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcDoubleFault();
}

extern "C" void ExcCoprocessorSegmentOverrun()
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcCoprocessorSegmentOverrun();
}

extern "C" void ExcInvalidTaskStateSegment()
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcInvalidTaskStateSegment();
}

extern "C" void ExcSegmentNotPresent()
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcSegmentNotPresent();
}

extern "C" void ExcStackFault()
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcStackFault();
}

extern "C" void ExcGeneralProtectionFault()
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcGeneralProtectionFault();
}

extern "C" void ExcPageFault()
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcPageFault();
}

extern "C" void ExcReserved()
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcReserved();
}

extern "C" void ExcMathFault()
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcMathFault();
}

extern "C" void ExcAlignmentCheck()
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcAlignmentCheck();
}

extern "C" void ExcMachineCheck()
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcMachineCheck();
}

extern "C" void ExcSIMDFpException()
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcSIMDFpException();
}

extern "C" void ExcVirtException()
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcVirtException();
}

extern "C" void ExcControlProtection()
{
    auto& excTable = ExceptionTable::GetInstance();

    excTable.ExcControlProtection();
}

}
}