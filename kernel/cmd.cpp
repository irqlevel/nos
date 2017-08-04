#include "cmd.h"
#include "trace.h"
#include "asm.h"
#include "dmesg.h"
#include "cpu.h"
#include "time.h"
#include "watchdog.h"

#include <drivers/vga.h>

namespace Kernel
{

Cmd::Cmd()
    : Task(nullptr)
    , Exit(false)
    , Active(false)
{
    CmdLine[0] = '\0';
}

Cmd::~Cmd()
{
    if (Task != nullptr)
    {
        Task->Put();
        Task = nullptr;
    }
}

void Cmd::ProcessCmd(const char *cmd)
{
    auto& vga = VgaTerm::GetInstance();

    if (Stdlib::StrCmp(cmd, "cls") == 0)
    {
        vga.Cls();
    }
    else if (Stdlib::StrCmp(cmd, "exit") == 0 ||
             Stdlib::StrCmp(cmd, "quit") == 0)
    {
        Exit = true;
        return;
    }
    else if (Stdlib::StrCmp(cmd, "cpu") == 0)
    {
        vga.Printf("ss 0x%p cs 0x%p ds 0x%p gs 0x%p fs 0x%p es 0x%p",
            (ulong)GetSs(), (ulong)GetCs(), (ulong)GetDs(),
            (ulong)GetGs(), (ulong)GetFs(), (ulong)GetEs());

        vga.Printf("rflags 0x%p rsp 0x%p rip 0x%p\n",
            GetRflags(), GetRsp(), GetRip());

        vga.Printf("cr0 0x%p cr2 0x%p cr3 0x%p cr4 0x%p",
            GetCr0(), GetCr2(), GetCr3(), GetCr4());
    }
    else if (Stdlib::StrCmp(cmd, "dmesg") == 0)
    {
        Dmesg::GetInstance().Dump(vga);
    }
    else if (Stdlib::StrCmp(cmd, "uptime") == 0)
    {
        auto time = GetBootTime();
        vga.Printf("%u.%u\n", time.GetSecs(), time.GetUsecs());
    }
    else if (Stdlib::StrCmp(cmd, "ps") == 0)
    {
        TaskTable::GetInstance().Ps(vga);
    }
    else if (Stdlib::StrCmp(cmd, "watchdog") == 0)
    {
        Watchdog::GetInstance().Dump(vga);
    }
    else if (Stdlib::StrCmp(cmd, "help") == 0)
    {
        vga.Printf("cls - clear screen\n");
        vga.Printf("cpu - dump cpu state\n");
        vga.Printf("dmesg - dump kernel log\n");
        vga.Printf("exit - shutdown kernel\n");
        vga.Printf("ps - show tasks\n");
        vga.Printf("watchdog - show watchdog stats\n");
        vga.Printf("help - help\n");
    }
    else
    {
        vga.Printf("command not found\n");
    }
    vga.Printf("$");
}

bool Cmd::IsExit()
{
    return Exit;
}

void Cmd::Stop()
{
    if (Active)
    {
        BugOn(Task == nullptr);
        Active = false;
        Task->SetStopping();
        Task->Wait();
    }
}

bool Cmd::Start()
{
    if (Task != nullptr)
        return false;

    auto task = new class Task("cmd");
    if (task == nullptr)
        return false;

    {
        Stdlib::AutoLock lock(Lock);
        if (Task == nullptr)
        {
            Task = task;
        }
    }

    if (Task != task)
    {
        task->Put();
        return false;
    }

    if (!Task->Start(&Cmd::RunFunc, this))
    {
        {
            Stdlib::AutoLock lock(Lock);
            task = Task;
            Task = nullptr;
        }
        task->Put();
        return false;
    }

    {
        Stdlib::AutoLock lock(Lock);
        VgaTerm::GetInstance().Printf("\n$");
        Active = true;
    }
    return true;
}

void Cmd::Run()
{
    size_t pos = 0;
    bool overflow = false;

    auto& vga = VgaTerm::GetInstance();

    while (!Task::GetCurrentTask()->IsStopping())
    {
        char c;
        bool hasChar = false;
        {
            Stdlib::AutoLock lock(Lock);
            if (!Buf.IsEmpty())
            {
                c = Buf.Get();
                hasChar = true;
            }
        }

        if (hasChar)
        {
            vga.Printf("%c", c);
            if (c == '\n')
            {
                CmdLine[Stdlib::ArraySize(CmdLine) - 1] = '\0';
                if (!overflow)
                {
                    ProcessCmd(CmdLine);
                }
                else
                {
                    vga.Printf("command too large\n");
                    vga.Printf("$");
                    overflow = false;
                }
                Stdlib::MemSet(CmdLine, 0, Stdlib::StrLen(CmdLine));
                pos = 0;
            }
            else
            {
                if (pos < (Stdlib::ArraySize(CmdLine) - 1))
                    CmdLine[pos++] = c;
                else
                {
                    overflow = true;
                }
            }
        }

        Sleep(10 * Const::NanoSecsInMs);
    }
}

void Cmd::RunFunc(void *ctx)
{
    Cmd* cmd = static_cast<Cmd*>(ctx);
    cmd->Run();
}

void Cmd::OnChar(char c)
{
    Stdlib::AutoLock lock(Lock);
    if (!Active)
        return;

    if (!Buf.Put(c))
    {
        Trace(0, "Can't save char");
        return;
    }
}

}