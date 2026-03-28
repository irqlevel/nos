#pragma once

#include "stdlib.h"

namespace Stdlib
{

class Printer
{
public:
    virtual void Printf(const char *fmt, ...) = 0;
    virtual void VPrintf(const char *fmt, va_list args) = 0;
    virtual void PrintString(const char *s) = 0;
    virtual void Backspace() = 0;
};

template <typename T>
class TypePrinter
{
public:
    virtual void PrintElement(const T& element) = 0;
};

};
