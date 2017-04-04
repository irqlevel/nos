#include "stdlib.h"
#include "8042.h"
#include "ring_buffer.h"
#include "spin_lock.h"

namespace Kernel
{

namespace Core
{

class Cmd final : public IO8042Observer
{
public:
    static Cmd& GetInstance()
    {
        static Cmd instance;
        return instance;
    }

    void Run();

    virtual void OnChar(char c) override;

    bool IsExit();

    void Start();

private:
    void ProcessCmd(const char *cmd);

    Cmd();
    ~Cmd();
    Cmd(const Cmd& other) = delete;
    Cmd(Cmd&& other) = delete;
    Cmd& operator=(const Cmd& other) = delete;
    Cmd& operator=(Cmd&& other) = delete;

    static const size_t CmdSizeMax = 80;
    Shared::RingBuffer<char, CmdSizeMax> Buf;
    char CmdLine[CmdSizeMax + 1];
    SpinLock Lock;

    bool Exit;
    bool Active;
};

}
}