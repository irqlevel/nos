#pragma once

#include "types.h"

namespace Kernel
{

namespace Core
{

class IO8042
{
public:
    static IO8042& GetInstance()
    {
        static IO8042 instance;
        return instance;
    }

private:
    IO8042();
    virtual ~IO8042();

    IO8042(const IO8042& other) = delete;
    IO8042(IO8042&& other) = delete;
    IO8042& operator=(const IO8042& other) = delete;
    IO8042& operator=(IO8042&& other) = delete;

    __attribute((interrupt)) static void Interrupt(void *frame);

    const ulong Port = 0x80;

    const ulong BufSize = 16;
    u8 *Buf;
    u8 *BufPtr;
};

}
}
