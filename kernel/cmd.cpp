#include "cmd.h"
#include "trace.h"
#include "asm.h"
#include "dmesg.h"

#include <drivers/vga.h>

namespace Kernel
{

namespace Core
{

Cmd::Cmd()
    : Exit(false)
    , Active(false)
{
    CmdLine[0] = '\0';
}

Cmd::~Cmd()
{
}

void Cmd::ProcessCmd(const char *cmd)
{
    auto& vga = VgaTerm::GetInstance();

    if (Shared::StrCmp(cmd, "cls\n") == 0)
    {
        vga.Cls();
    }
    else if (Shared::StrCmp(cmd, "exit\n") == 0 ||
             Shared::StrCmp(cmd, "quit\n") == 0)
    {
        Exit = true;
        return;
    }
    else if (Shared::StrCmp(cmd, "cpu\n") == 0)
    {
        vga.Printf("ss 0x%p cs 0x%p ds 0x%p gs 0x%p fs 0x%p es 0x%p\n",
            (ulong)GetSs(), (ulong)GetCs(), (ulong)GetDs(),
            (ulong)GetGs(), (ulong)GetFs(), (ulong)GetEs());

        vga.Printf("rflags 0x%p rsp 0x%p rip 0x%p\n",
            GetRflags(), GetRsp(), GetRip());

        vga.Printf("cr0 0x%p cr2 0x%p cr3 0x%p cr4 0x%p\n",
            GetCr0(), GetCr2(), GetCr3(), GetCr4());
    }
    else if (Shared::StrCmp(cmd, "dmesg\n") == 0)
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
    else if (Shared::StrCmp(cmd, "uptime\n") == 0)
    {
        auto time = Pit::GetInstance().GetTime();
        vga.Printf("%u.%u\n", time.Secs, time.NanoSecs);
    }
    else if (Shared::StrCmp(cmd, "help\n") == 0)
    {
        vga.Printf("cls - clear screen\n");
        vga.Printf("cpu - dump cpu state\n");
        vga.Printf("dmesg - dump kernel log\n");
        vga.Printf("exit - shutdown kernel\n");
        vga.Printf("help - help\n");
    }
    else if (Shared::StrCmp(cmd, "\n") == 0)
    {
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

void Cmd::Start()
{
    Shared::AutoLock lock(Lock);

    auto& vga = VgaTerm::GetInstance();

    vga.Printf("\n$");
    Active = true;
}

void Cmd::Run()
{
    Shared::AutoLock lock(Lock);
    if (!Active)
        return;

    if (CmdLine[0] != '\0')
    {
        ProcessCmd(CmdLine);
        CmdLine[0] = '\0';
    }
}

void Cmd::OnChar(char c)
{
    Shared::AutoLock lock(Lock);
    if (!Active)
        return;

    if (!Buf.Put(c))
    {
        Trace(0, "Can't save cmd char, drop command");
        Buf.Clear();
        goto output;
    }

    if (c == '\n')
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
    }

output:
    VgaTerm::GetInstance().Printf("%c", c);
}

}
}