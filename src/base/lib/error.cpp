#include "error.h"

namespace stdlib
{

Error::Error()
    : func_("unknown")
    , file_("unknown")
    , line_(0)
    , code_(SUCCESS)
{
}

Error::Error(int code)
    : Error()
{
    code_ = code;
}

Error::Error(int code, const char *func, const char *file, int line)
    : Error()
{
    code_ = code;
    func_ = func;
    file_ = file;
    line_ = line;
}

Error::~Error()
{
}

int Error::get_code() const
{
    return code_;
}

void Error::set_code(int code)
{
    code_ = code;
}

const char* Error::get_description() const
{
    switch (code_)
    {
    case SUCCESS:
        return "Success";
    case INVALID_VALUE:
        return "Invalid value";
    case NO_MEMORY:
        return "No memory";
    case CANCELLED:
        return "Cancelled";
    default:
        return "Unknown";
    }
}

bool Error::operator!= (const Error& other) const
{
    return get_code() != other.get_code();
}

bool Error::operator== (const Error& other) const
{
    return get_code() == other.get_code();
}

void Error::reset()
{
    code_ = SUCCESS;
}

bool Error::ok() const
{
    return (code_ == SUCCESS) ? true : false;
}

int Error::get_line() const
{
    return line_;
}

const char* Error::get_file() const
{
    return file_;
}

const char* Error::get_func() const
{
    return func_;
}

Error::Error(const Error& other)
    : Error()
{
    code_ = other.code_;
    func_ = other.func_;
    file_ = other.file_;
    line_ = other.line_;
}

Error::Error(Error&& other)
{
    code_ = other.code_;
    func_ = other.func_;
    file_ = other.file_;
    line_ = other.line_;
    other.reset();
}

Error& Error::operator=(const Error& other)
{
    if (this != &other)
    {
        code_ = other.code_;
        func_ = other.func_;
        file_ = other.file_;
        line_ = other.line_;
    }
    return *this;
}

Error& Error::operator=(Error&& other)
{
    if (this != &other)
    {
        code_ = other.code_;
        func_ = other.func_;
        file_ = other.file_;
        line_ = other.line_;
        other.reset();
    }
    return *this;
}

}