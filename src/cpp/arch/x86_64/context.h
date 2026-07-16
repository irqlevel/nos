#pragma once

#include <include/types.h>

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

    ulong GetFramePointer()
    {
        return Rbp;
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

}
