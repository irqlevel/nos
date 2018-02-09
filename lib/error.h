#pragma once

#include "stdlib.h"

namespace Stdlib
{

class Error final
{
public:
    Error();

    Error(int code);

    Error(int code, const char *func, const char *file, int line);

    ~Error();

    int GetCode() const;

    void SetCode(int code);

    const char* GetDescription() const;

    static const int Success = 0;

    static const int Again = 11;

    static const int InvalidValue = 22;

    static const int NoMemory = 12;

    static const int ConnReset = 104;

    static const int Cancelled = 125;

    static const int NotExecuted = 500;

    static const int InvalidState = 501;

    static const int Unsuccessful = 502;

    static const int NotImplemented = 503;

    static const int UnknownCode = 504;

    static const int NotFound = 505;

    static const int EOF = 506;

    static const int BufToBig = 507;

    static const int IO = 508;

    static const int BadMagic = 509;

    static const int HeaderCorrupt = 510;

    static const int DataCorrupt = 511;

    static const int BadSize = 512;

    static const int AlreadyExists = 513;

    static const int UnexpectedEOF = 514;

    static const int Overflow = 515;

    static const int Overlap = 516;

    bool operator!= (const Error& other) const;

    bool operator== (const Error& other) const;

    bool Ok() const;

    void Reset();

    const char* GetFile() const;

    const char* GetFunc() const;

    int GetLine() const;

    Error(const Error& other);

    Error(Error&& other);

    Error& operator=(const Error& other);

    Error& operator=(Error&& other);

private:
    const char* Func;
    const char* File;
    int Line;
    int Code;
};

}

#define MakeError(code) \
    Stdlib::Error(code, __FUNCTION__, Stdlib::TruncateFileName(__FILE__), __LINE__)

#define MakeSuccess() \
    Stdlib::Error(Stdlib::Error::Success, __FUNCTION__, Stdlib::TruncateFileName(__FILE__), __LINE__)