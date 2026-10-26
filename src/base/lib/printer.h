#pragma once

#include "stdlib.h"

namespace stdlib
{

/* A terminal, as its client last said: its width in columns and height in
   rows (0 where it did not say), and how many times it has said they
   changed. kcore's CmdTerminal on the Rust side: three 32-bit words. */
struct TerminalInfo
{
    unsigned int cols;
    unsigned int rows;
    unsigned int changes;
};

class Printer
{
public:
    virtual PRINTF_FORMAT(2, 3) void printf(const char *fmt, ...) = 0;
    virtual PRINTF_FORMAT(2, 0) void v_printf(const char *fmt, va_list args) = 0;
    virtual void print_string(const char *s) = 0;
    virtual void backspace() = 0;

    /* What the person at the other end types, for a command that reads it
       while it runs: up to len bytes, waiting up to timeout_ns for some.
       0 when the time passed with nothing, -1 when no one can type here --
       the console, the UDP shell, /etc/rc -- or no more will come. Only an
       SSH session's printer has anyone to ask (kernel_cmd_dispatch_io). */
    virtual long read_input(unsigned char* buf, unsigned long len, unsigned long long timeout_ns)
    {
        (void)buf;
        (void)len;
        (void)timeout_ns;
        return -1;
    }

    /* The terminal of the person at the other end, for a command that
       draws on it: false, info untouched, where there is none. Only an SSH
       session's printer knows one (kernel_cmd_dispatch_io). */
    virtual bool terminal(TerminalInfo& info)
    {
        (void)info;
        return false;
    }
};

/* A Printer into a buffer its owner provides: what a command printed, kept
   to be read back or passed on. What does not fit is dropped, and the text
   is NUL-terminated whatever happens. */
class BufferPrinter final : public Printer
{
public:
    BufferPrinter(char* buf, size_t size)
        : buf_(buf)
        , size_(size)
        , pos_(0)
    {
        if (size_ != 0)
            buf_[0] = '\0';
    }

    virtual PRINTF_FORMAT(2, 3) void printf(const char *fmt, ...) override
    {
        va_list args;
        va_start(args, fmt);
        v_printf(fmt, args);
        va_end(args);
    }

    virtual PRINTF_FORMAT(2, 0) void v_printf(const char *fmt, va_list args) override
    {
        if (pos_ + 1 >= size_)
            return;

        vsn_printf(buf_ + pos_, size_ - pos_, fmt, args);
        buf_[size_ - 1] = '\0';
        pos_ += str_len(buf_ + pos_);
    }

    virtual void print_string(const char *s) override
    {
        if (pos_ + 1 >= size_)
            return;

        size_t len = str_len(s);
        if (len > size_ - 1 - pos_)
            len = size_ - 1 - pos_;
        mem_cpy(buf_ + pos_, s, len);
        pos_ += len;
        buf_[pos_] = '\0';
    }

    virtual void backspace() override
    {
    }

    const char* get() const
    {
        return buf_;
    }

    void reset()
    {
        pos_ = 0;
        if (size_ != 0)
            buf_[0] = '\0';
    }

private:
    char* buf_;
    size_t size_;
    size_t pos_;
};

template <typename T>
class TypePrinter
{
public:
    virtual void print_element(const T& element) = 0;
};

};
