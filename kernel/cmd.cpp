#include "cmd.h"
#include "trace.h"
#include "asm.h"
#include "dmesg.h"
#include "cpu.h"

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
        delete Task;
        Task = nullptr;
    }
}

void Cmd::ProcessCmd(const char *cmd)
{
    auto& vga = VgaTerm::GetInstance();

    if (Shared::StrCmp(cmd, "cls") == 0)
    {
        vga.Cls();
    }
    else if (Shared::StrCmp(cmd, "exit") == 0 ||
             Shared::StrCmp(cmd, "quit") == 0)
    {
        Exit = true;
        return;
    }
    else if (Shared::StrCmp(cmd, "cpu") == 0)
    {
        vga.Printf("ss 0x%p cs 0x%p ds 0x%p gs 0x%p fs 0x%p es 0x%p",
            (ulong)GetSs(), (ulong)GetCs(), (ulong)GetDs(),
            (ulong)GetGs(), (ulong)GetFs(), (ulong)GetEs());

        vga.Printf("rflags 0x%p rsp 0x%p rip 0x%p\n",
            GetRflags(), GetRsp(), GetRip());

        vga.Printf("cr0 0x%p cr2 0x%p cr3 0x%p cr4 0x%p",
            GetCr0(), GetCr2(), GetCr3(), GetCr4());
    }
    else if (Shared::StrCmp(cmd, "dmesg") == 0)
    {
        class DmesgPrinter final : public Dmesg::Dumper
        {
        public:
            DmesgPrinter(){}
            ~DmesgPrinter(){}
            virtual void OnMessage(const char *msg)
            {
                VgaTerm::GetInstance().Printf(msg);
            }
        };
        DmesgPrinter printer;
        Dmesg::GetInstance().Dump(printer);
    }
    else if (Shared::StrCmp(cmd, "uptime") == 0)
    {
        auto time = Pit::GetInstance().GetTime();
        vga.Printf("%u.%u\n", time.Secs, time.NanoSecs);
    }
    else if (Shared::StrCmp(cmd, "help") == 0)
    {
        vga.Printf("cls - clear screen\n");
        vga.Printf("cpu - dump cpu state\n");
        vga.Printf("dmesg - dump kernel log\n");
        vga.Printf("exit - shutdown kernel\n");
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

    auto task = new class Task();
    if (task == nullptr)
        return false;

    {
        Shared::AutoLock lock(Lock);
        if (Task == nullptr)
        {
            Task = task;
        }
    }

    if (Task != task)
    {
        delete task;
        return false;
    }

    if (!Task->Start(&Cmd::RunFunc, this))
    {
        {
            Shared::AutoLock lock(Lock);
            task = Task;
            Task = nullptr;
        }
        delete task;
        return false;
    }

    {
        Shared::AutoLock lock(Lock);
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
            Shared::AutoLock lock(Lock);
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
                CmdLine[Shared::ArraySize(CmdLine) - 1] = '\0';
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
                Shared::MemSet(CmdLine, 0, Shared::StrLen(CmdLine));
                pos = 0;
            }
            else
            {
                if (pos < (Shared::ArraySize(CmdLine) - 1))
                    CmdLine[pos++] = c;
                else
                {
                    overflow = true;
                }
            }
        }

        GetCpu().Sleep(10000000);
    }
}

void Cmd::RunFunc(void *ctx)
{
    Cmd* cmd = static_cast<Cmd*>(ctx);
    cmd->Run();
}

void Cmd::OnChar(char c)
{
    Shared::AutoLock lock(Lock);
    if (!Active)
        return;

    if (!Buf.Put(c))
    {
        Trace(0, "Can't save char");
        return;
    }
}

}