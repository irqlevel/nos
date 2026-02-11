#include "spin_lock.h"
#include "task.h"

#include <drivers/8042.h>
#include <drivers/serial.h>
#include <lib/stdlib.h>
#include <lib/ring_buffer.h>
#include <lib/printer.h>

namespace Kernel
{

class Cmd final
    : public IO8042Observer
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

    bool Start();

    void Stop();
    void StopDhcp();

private:
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
    static const ulong Tag = 'Cmd ';
};

}