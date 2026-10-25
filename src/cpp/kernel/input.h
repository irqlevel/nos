#pragma once

#include <include/types.h>
#include "spin_lock.h"

namespace Kernel
{

/* Source-agnostic keyboard sink. The legacy 8042 driver and the USB HID
   boot-keyboard driver both publish decoded characters here, so consumers
   (the shell) do not care which controller a key came from. What they send
   is what a terminal would for the key: its character, BS for Backspace,
   the control byte for Ctrl and a letter (^R is 0x12), and an escape
   sequence a byte at a time for an arrow (ESC [ A is Up) -- so the shell's
   editor reads a keyboard as it reads a serial port. `code` is the key's
   PS/2 set-1 make code, the same for every byte of a sequence. */
class KeyboardObserver
{
public:
    virtual void OnChar(char c, u8 code) = 0;
};

class KeyboardInput final
{
public:
    static KeyboardInput& GetInstance()
    {
        static KeyboardInput Instance;

        return Instance;
    }

    bool RegisterObserver(KeyboardObserver& observer);
    void UnregisterObserver(KeyboardObserver& observer);

    /* Deliver one decoded character to every registered observer. Safe from
       task context (USB poll task) and from timer context (8042 tick). */
    void Emit(char c, u8 code);

private:
    KeyboardInput();
    ~KeyboardInput();

    KeyboardInput(const KeyboardInput& other) = delete;
    KeyboardInput(KeyboardInput&& other) = delete;
    KeyboardInput& operator=(const KeyboardInput& other) = delete;
    KeyboardInput& operator=(KeyboardInput&& other) = delete;

    static const size_t MaxObserver = 16;

    SpinLock Lock;
    KeyboardObserver* Observer[MaxObserver];
};

}
