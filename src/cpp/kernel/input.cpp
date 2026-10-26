#include "input.h"

#include <lib/stdlib.h>

namespace kernel
{

KeyboardInput::KeyboardInput()
{
    for (size_t i = 0; i < stdlib::array_size(observer_); i++)
        observer_[i] = nullptr;
}

KeyboardInput::~KeyboardInput()
{
}

bool KeyboardInput::register_observer(KeyboardObserver& observer)
{
    stdlib::AutoLock lock(lock_);

    for (size_t i = 0; i < MAX_OBSERVER; i++)
    {
        if (observer_[i] == nullptr)
        {
            observer_[i] = &observer;
            return true;
        }
    }

    return false;
}

void KeyboardInput::unregister_observer(KeyboardObserver& observer)
{
    stdlib::AutoLock lock(lock_);

    for (size_t i = 0; i < MAX_OBSERVER; i++)
    {
        if (observer_[i] == &observer)
            observer_[i] = nullptr;
    }
}

void KeyboardInput::emit(char c, u8 code)
{
    stdlib::AutoLock lock(lock_);

    for (size_t i = 0; i < MAX_OBSERVER; i++)
    {
        if (observer_[i] != nullptr)
            observer_[i]->on_char(c, code);
    }
}

}
