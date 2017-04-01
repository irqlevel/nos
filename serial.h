#pragma once

#include "types.h"
#include "idt_descriptor.h"

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

    void Register(IdtDescriptor *irq);
    void Unregister();

    static void Interrupt();

private:
    Serial();
    ~Serial();

    Serial(const Serial& other) = delete;
    Serial(Serial&& other) = delete;
    Serial& operator=(const Serial& other) = delete;
    Serial& operator=(Serial&& other) = delete;

    void OnInterrupt();
    bool IsTransmitEmpty();
    IdtDescriptor *Irq;

    static const int Port = 0x3F8;
};


};

}