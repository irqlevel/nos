#pragma once

#include "spin_lock.h"
#include "task.h"

#include "input.h"

#include <drivers/serial.h>
#include <lib/stdlib.h>
#include <lib/ring_buffer.h>
#include <lib/printer.h>

namespace Kernel
{

class Cmd final
    : public KeyboardObserver
    , public SerialObserver
{
public:
    static Cmd& GetInstance()
    {
        static Cmd Instance;
        return Instance;
    }

    virtual void OnChar(char c, u8 code) override;

    bool ShouldShutdown();
    bool ShouldReboot();

    void RequestShutdown();
    void RequestReboot();

    bool Start();

    void Stop();
    void StopDhcp();

    static void Dispatch(const char *cmd, Stdlib::Printer& out);

    /* Shell commands added at run time, by a loadable module through
       kernel_cmd_register. A call hands the handler its ctx, the rest of the
       command line after the name, and the Stdlib::Printer to answer on --
       opaque to the module, which writes to it with kernel_printer_write. */
    typedef void (*DynamicHandler)(void* ctx, const char* args, ulong argsLen, void* out);

    /* A handle for UnregisterDynamic, or 0: the name is empty, too long or
       not one printable word, it is taken -- by a built-in command or by
       another module's -- or the table is full. The name and help are
       copied. */
    ulong RegisterDynamic(const char* name, ulong nameLen, const char* help, ulong helpLen,
        DynamicHandler handler, void* ctx);

    /* Returns once no call to the command is running and none can start:
       from then on its handler and ctx are the caller's to free. Sleeps while
       a call runs, so task context only -- and never from that command's own
       handler, which would wait for itself. */
    void UnregisterDynamic(ulong handle);

    /* The help lines of the commands added at run time */
    void DynamicHelp(Stdlib::Printer& out);

private:
    bool DispatchDynamic(const char* cmd, Stdlib::Printer& out);

    void ProcessCmd(const char *cmd);

    Cmd();
    ~Cmd();
    Cmd(const Cmd& other) = delete;
    Cmd(Cmd&& other) = delete;
    Cmd& operator=(const Cmd& other) = delete;
    Cmd& operator=(Cmd&& other) = delete;

    void ShowBanner(Stdlib::Printer& out);
    void Run();
    static void RunFunc(void *ctx);

    static const size_t CmdSizeMax = 80;

    struct KeyEvent {
        char Char;
        u8 Code;
    };

    Stdlib::RingBuffer<KeyEvent, Const::PageSize> Buf;
    char CmdLine[CmdSizeMax + 1];
    SpinLock Lock;
    Task *TaskPtr;
    bool Shutdown;
    bool Reboot;
    bool Active;

    static const ulong DynamicMax = 32;
    static const ulong DynamicNameMax = 31;
    static const ulong DynamicHelpMax = 95;
    /* A handle is the slot index + 1 in its low bits, and a registration
       count above them, so a stale handle never names the slot's next user */
    static const ulong DynamicSlotBits = 8;
    static_assert(DynamicMax < (1UL << DynamicSlotBits), "slot must fit a handle");
    static const ulong DynamicPollNs = Const::NanoSecsInMs;

    struct DynamicCmd
    {
        ulong Handle;       /* 0: the slot is free */
        char Name[DynamicNameMax + 1];
        char Help[DynamicHelpMax + 1];
        DynamicHandler Handler;
        void* Ctx;
        ulong Running;      /* calls in progress */
        bool Removing;      /* no call may start */
    };

    DynamicCmd Dynamic[DynamicMax];
    ulong DynamicGeneration;
    SpinLock DynamicLock;

    static const ulong Tag = 'Cmd ';
};

}