#include "cmd.h"
#include "trace.h"
#include "vga.h"

namespace Kernel
{

namespace Core
{

Cmd::Cmd()
    : InputActive(false)
    , Exit(false)
{
    CmdLine[0] = '\0';
}

Cmd::~Cmd()
{
}

void Cmd::ProcessCmd(const char *cmd)
{
    Trace(CmdLL, "Process cmd %s", cmd);

    auto& vga = VgaTerm::GetInstance();
    if (Shared::StrCmp(cmd, "cls") == 0)
    {
        vga.Cls();
    }
    else if (Shared::StrCmp(cmd, "exit") == 0)
    {
        Exit = true;
    }
    else
    {
        vga.Printf("Unknown command\n");
    }
}

bool Cmd::IsExit()
{
    return Exit;
}

void Cmd::Run()
{
    Shared::AutoLock lock(Lock);

    if (CmdLine[0] != '\0')
    {
        ProcessCmd(CmdLine);
        CmdLine[0] = '\0';
    }
}

void Cmd::OnChar(char c)
{
    Shared::AutoLock lock(Lock);

    if (c == '.')
    {
        InputActive = true;
        return;
    }

    if (InputActive)
    {
        if (c == 0xA) //CR
        {
            if (CmdLine[0] == '\0')
            {
                size_t i = 0;
                while (!Buf.IsEmpty())
                {
                    BugOn(i >= (Shared::ArraySize(CmdLine) - 1));
                    CmdLine[i] = Buf.Get();
                    i++;
                }
                CmdLine[i] = '\0';
            }
            else
            {
                Trace(0, "Can't save cmd, drop command");
                Buf.Clear();
            }
            InputActive = false;
        }
        else
        {
            if (!Buf.Put(c))
            {
                Trace(0, "Can't save cmd char, drop command");
                Buf.Clear();
                InputActive = false;
            }
        }
    }
}

}
}