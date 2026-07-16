#pragma once

#include <include/types.h>
#include <kernel/interrupt.h>

namespace Kernel
{

class GenericTimer final : public InterruptHandler
{
public:
    static GenericTimer& GetInstance()
    {
        static GenericTimer Instance;
        return Instance;
    }

    bool Setup();

    void OnInterruptRegister(u8 irq, u8 vector) override;
    InterruptHandlerFn GetHandlerFn() override;
    void OnInterrupt(Context* ctx) override;

private:
    GenericTimer();
    ~GenericTimer();
    GenericTimer(const GenericTimer& other) = delete;
    GenericTimer(GenericTimer&& other) = delete;
    GenericTimer& operator=(const GenericTimer& other) = delete;
    GenericTimer& operator=(GenericTimer&& other) = delete;

    u8 IntVector;
    u64 TickInterval;
};

}
