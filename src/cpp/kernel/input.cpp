#include "input.h"

#include <lib/stdlib.h>

namespace Kernel
{

KeyboardInput::KeyboardInput()
{
    for (size_t i = 0; i < Stdlib::ArraySize(Observer); i++)
        Observer[i] = nullptr;
}

KeyboardInput::~KeyboardInput()
{
}

bool KeyboardInput::RegisterObserver(KeyboardObserver& observer)
{
    Stdlib::AutoLock lock(Lock);

    for (size_t i = 0; i < MaxObserver; i++)
    {
        if (Observer[i] == nullptr)
        {
            Observer[i] = &observer;
            return true;
        }
    }

    return false;
}

void KeyboardInput::UnregisterObserver(KeyboardObserver& observer)
{
    Stdlib::AutoLock lock(Lock);

    for (size_t i = 0; i < MaxObserver; i++)
    {
        if (Observer[i] == &observer)
            Observer[i] = nullptr;
    }
}

void KeyboardInput::Emit(char c, u8 code)
{
    Stdlib::AutoLock lock(Lock);

    for (size_t i = 0; i < MaxObserver; i++)
    {
        if (Observer[i] != nullptr)
            Observer[i]->OnChar(c, code);
    }
}

}
