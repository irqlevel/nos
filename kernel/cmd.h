#include "spin_lock.h"
#include "task.h"

#include <drivers/8042.h>
#include <lib/stdlib.h>
#include <lib/ring_buffer.h>
#include <lib/printer.h>

namespace Kernel
{

class Cmd final
    : public IO8042Observer
{
public:
    static Cmd& GetInstance()
    {
        static Cmd Instance;
        return Instance;
    }

    virtual void OnChar(char c, u8 code) override;

    bool IsExit();

    bool Start();

    void Stop();

private:
    void ProcessCmd(const char *cmd);

    Cmd();
    ~Cmd();
    Cmd(const Cmd& other) = delete;
    Cmd(Cmd&& other) = delete;
    Cmd& operator=(const Cmd& other) = delete;
    Cmd& operator=(Cmd&& other) = delete;

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
    Task *Task;
    bool Exit;
    bool Active;
};

}