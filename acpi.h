#pragma once

#include "stdlib.h"

namespace Kernel
{

namespace Core
{

class Acpi final
{
public:
    static Acpi& GetInstance()
    {
        static Acpi instance;
        return instance;
    }

    void* GetRSDP();

private:
    Acpi();
    ~Acpi();
    Acpi(const Acpi& other) = delete;
    Acpi(Acpi&& other) = delete;
    Acpi& operator=(const Acpi& other) = delete;
    Acpi& operator=(Acpi&& other) = delete;
};

}

}