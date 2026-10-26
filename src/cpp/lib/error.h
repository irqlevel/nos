#pragma once

#include "stdlib.h"

namespace stdlib
{

class Error final
{
public:
    Error();

    Error(int code);

    Error(int code, const char *func, const char *file, int line);

    ~Error();

    int get_code() const;

    void set_code(int code);

    const char* get_description() const;

    static const int SUCCESS = 0;

    static const int AGAIN = 11;

    static const int INVALID_VALUE = 22;

    static const int NO_MEMORY = 12;

    static const int CONN_RESET = 104;

    static const int CANCELLED = 125;

    static const int NOT_EXECUTED = 500;

    static const int INVALID_STATE = 501;

    static const int UNSUCCESSFUL = 502;

    static const int NOT_IMPLEMENTED = 503;

    static const int UNKNOWN_CODE = 504;

    static const int NOT_FOUND = 505;

    static const int EOF = 506;

    static const int BUF_TOO_BIG = 507;

    static const int IO = 508;

    static const int BAD_MAGIC = 509;

    static const int HEADER_CORRUPT = 510;

    static const int DATA_CORRUPT = 511;

    static const int BAD_SIZE = 512;

    static const int ALREADY_EXISTS = 513;

    static const int UNEXPECTED_EOF = 514;

    static const int OVERFLOWED = 515;

    static const int OVERLAP = 516;

    bool operator!= (const Error& other) const;

    bool operator== (const Error& other) const;

    bool ok() const;

    void reset();

    const char* get_file() const;

    const char* get_func() const;

    int get_line() const;

    Error(const Error& other);

    Error(Error&& other);

    Error& operator=(const Error& other);

    Error& operator=(Error&& other);

private:
    const char* func_;
    const char* file_;
    int line_;
    int code_;
};

}

#define make_error(code) \
    stdlib::Error(code, __FUNCTION__, stdlib::truncate_file_name(__FILE__), __LINE__)

#define make_success() \
    stdlib::Error(stdlib::Error::SUCCESS, __FUNCTION__, stdlib::truncate_file_name(__FILE__), __LINE__)