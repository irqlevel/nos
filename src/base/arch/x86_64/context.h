#pragma once

#include <include/types.h>

namespace kernel
{

struct JmpContext final
{
    ulong rbx;
    ulong rsp;
    ulong rbp;
    ulong r12;
    ulong r13;
    ulong r14;
    ulong r15;
    ulong ret_addr;
};

struct Context final
{
    ulong r15;
    ulong r14;
    ulong r13;
    ulong r12;
    ulong r11;
    ulong r10;
    ulong r9;
    ulong r8;
    ulong rsi;
    ulong rdi;
    ulong rdx;
    ulong rcx;
    ulong rbx;
    ulong rax;
    ulong rbp;
    ulong rflags;
    ulong rsp;

    ulong get_ret_rip(bool has_error_code = false)
    {
        if (has_error_code)
            return *((ulong *)(rsp + sizeof(ulong)));
        return *((ulong *)rsp);
    }

    ulong get_error_code()
    {
        return *((ulong *)rsp);
    }

    ulong get_frame_pointer()
    {
        return rbp;
    }

    ulong get_orig_rsp(bool has_error_code = false)
    {
        if (has_error_code)
            return *((ulong *)(rsp + 4 * sizeof(ulong)));
        return *((ulong *)(rsp + 3 * sizeof(ulong)));
    }
private:
    Context(const Context& other) = delete;
    Context(Context&& other) = delete;
    Context& operator=(const Context& other) = delete;
    Context& operator=(Context&& other) = delete;
};

}
