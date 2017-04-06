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

    void ExcDivideByZero();
    void ExcDebugger();
    void ExcNMI();
    void ExcBreakpoint();
    void ExcOverflow();
    void ExcBounds();
    void ExcInvalidOpcode();
    void ExcCoprocessorNotAvailable();
    void ExcDoubleFault();
    void ExcCoprocessorSegmentOverrun();
    void ExcInvalidTaskStateSegment();
    void ExcSegmentNotPresent();
    void ExcStackFault();
    void ExcGeneralProtectionFault();
    void ExcPageFault();
    void ExcReserved();
    void ExcMathFault();
    void ExcAlignmentCheck();
    void ExcMachineCheck();
    void ExcSIMDFpException();
    void ExcVirtException();
    void ExcControlProtection();

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