#pragma once

#include "types.h"

namespace Kernel
{

namespace Core
{

class Serial final
{
public:
    static Serial& GetInstance()
    {
        static Serial instance;

        return instance;
    }

    void WriteChar(char c);
    void Wait();

    void WriteString(const char *str);

    void Vprintf(const char *fmt, va_list args);
    void Printf(const char *fmt, ...);

private:
    Serial();
    ~Serial();

    Serial(const Serial& other) = delete;
    Serial(Serial&& other) = delete;
    Serial& operator=(const Serial& other) = delete;
    Serial& operator=(Serial&& other) = delete;

    bool IsTransmitEmpty();

    static const int Port = 0x3F8;
};


};

}