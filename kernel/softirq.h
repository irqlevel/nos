#pragma once

#include <include/types.h>
#include "atomic.h"
#include "task.h"

namespace Kernel
{

class SoftIrq
{
public:
    static SoftIrq& GetInstance()
    {
        static SoftIrq instance;
        return instance;
    }

    /* Called during boot: creates the soft IRQ task */
    bool Init();
    void Stop();

    /* Called from hard IRQ handler to schedule deferred work */
    void Raise(ulong type);

    /* Register a handler for a soft IRQ type */
    void Register(ulong type, void (*handler)(void* ctx), void* ctx);

    static const ulong TypeNetRx = 0;
    static const ulong TypeBlkIo = 1;
    static const ulong TypeNetTx = 2;
    static const ulong MaxTypes = 8;

private:
    SoftIrq();
    ~SoftIrq();
    SoftIrq(const SoftIrq& other) = delete;
    SoftIrq(SoftIrq&& other) = delete;
    SoftIrq& operator=(const SoftIrq& other) = delete;
    SoftIrq& operator=(SoftIrq&& other) = delete;

    struct Handler
    {
        void (*Func)(void* ctx);
        void* Ctx;
    };

    Handler Handlers[MaxTypes];
    Atomic Pending; /* bitmask of pending soft IRQ types */

    Task* TaskPtr;
    static void TaskFunc(void* ctx);
    void Run();

    static const ulong Tag = 'SIrq';
};

}
