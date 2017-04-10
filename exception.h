#pragma once

#include "types.h"
#include "atomic.h"

namespace Kernel
{

namespace Core
{

class ExceptionTable final
{

public:
    static ExceptionTable& GetInstance()
    {
        static ExceptionTable instance;

        return instance;
    }

    void RegisterExceptionHandlers();

    void ExcDivideByZero(Context* ctx);
    void ExcDebugger(Context* ctx);
    void ExcNMI(Context* ctx);
    void ExcBreakpoint(Context* ctx);
    void ExcOverflow(Context* ctx);
    void ExcBounds(Context* ctx);
    void ExcInvalidOpcode(Context* ctx);
    void ExcCoprocessorNotAvailable(Context* ctx);
    void ExcDoubleFault(Context* ctx);
    void ExcCoprocessorSegmentOverrun(Context* ctx);
    void ExcInvalidTaskStateSegment(Context* ctx);
    void ExcSegmentNotPresent(Context* ctx);
    void ExcStackFault(Context* ctx);
    void ExcGeneralProtectionFault(Context* ctx);
    void ExcPageFault(Context* ctx);
    void ExcReserved(Context* ctx);
    void ExcMathFault(Context* ctx);
    void ExcAlignmentCheck(Context* ctx);
    void ExcMachineCheck(Context* ctx);
    void ExcSIMDFpException(Context* ctx);
    void ExcVirtException(Context* ctx);
    void ExcControlProtection(Context* ctx);

private:
    ExceptionTable();
    ~ExceptionTable();

    ExceptionTable(const ExceptionTable& other) = delete;
    ExceptionTable(ExceptionTable& other) = delete;

    ExceptionTable& operator=(const ExceptionTable& other) = delete;
    ExceptionTable& operator=(ExceptionTable& other) = delete; 

    using ExcHandler = void (*)();

    bool SetHandler(size_t index, ExcHandler handler);

    ExcHandler Handler[0x16];

    Atomic ExcDivideByZeroCounter;
    Atomic ExcDebuggerCounter;
    Atomic ExcNMICounter;
    Atomic ExcBreakpointCounter;
    Atomic ExcOverflowCounter;
    Atomic ExcBoundsCounter;
    Atomic ExcInvalidOpcodeCounter;
    Atomic ExcCoprocessorNotAvailableCounter;
    Atomic ExcDoubleFaultCounter;
    Atomic ExcCoprocessorSegmentOverrunCounter;
    Atomic ExcInvalidTaskStateSegmentCounter;
    Atomic ExcSegmentNotPresentCounter;
    Atomic ExcStackFaultCounter;
    Atomic ExcGeneralProtectionFaultCounter;
    Atomic ExcPageFaultCounter;
    Atomic ExcReservedCounter;
    Atomic ExcMathFaultCounter;
    Atomic ExcAlignmentCheckCounter;
    Atomic ExcMachineCheckCounter;
    Atomic ExcSIMDFpExceptionCounter;
    Atomic ExcVirtExceptionCounter;
    Atomic ExcControlProtectionCounter;
};

}

}