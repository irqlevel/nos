#include "error.h"

namespace Stdlib
{

Error::Error()
    : Func(nullptr)
    , File(nullptr)
    , Line(0)
    , Code(Success)
{
}

Error::Error(int code)
    : Error()
{
    Code = code;
}

Error::Error(int code, const char *func, const char *file, int line)
    : Error()
{
    Code = code;
    Func = func;
    File = file;
    Line = line;
}

Error::~Error()
{
}

int Error::GetCode() const
{
    return Code;
}

void Error::SetCode(int code)
{
    Code = code;
}

const char* Error::GetDescription() const
{
    switch (Code)
    {
    case Success:
        return "Success";
    case InvalidValue:
        return "Invalid value";
    case NoMemory:
        return "No memory";
    case Cancelled:
        return "Cancelled";
    default:
        return "Unknown";
    }
}

bool Error::operator!= (const Error& other) const
{
    return GetCode() != other.GetCode();
}

bool Error::operator== (const Error& other) const
{
    return GetCode() == other.GetCode();
}

void Error::Reset()
{
    Code = Success;
}

bool Error::Ok() const
{
    return (Code == Success) ? true : false;
}

int Error::GetLine() const
{
    return Line;
}

const char* Error::GetFile() const
{
    return File;
}

const char* Error::GetFunc() const
{
    return Func;
}

Error::Error(const Error& other)
    : Error()
{
    Code = other.Code;
    Func = other.Func;
    File = other.File;
    Line = other.Line;
}

Error::Error(Error&& other)
{
    Code = other.Code;
    Func = other.Func;
    File = other.File;
    Line = other.Line;
    other.Reset();
}

Error& Error::operator=(const Error& other)
{
    if (this != &other)
    {
        Code = other.Code;
        Func = other.Func;
        File = other.File;
        Line = other.Line;
    }
    return *this;
}

Error& Error::operator=(Error&& other)
{
    if (this != &other)
    {
        Code = other.Code;
        Func = other.Func;
        File = other.File;
        Line = other.Line;
        other.Reset();
    }
    return *this;
}

}