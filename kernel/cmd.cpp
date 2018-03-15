#include "cmd.h"
#include "trace.h"
#include "asm.h"
#include "dmesg.h"
#include "cpu.h"
#include "time.h"
#include "watchdog.h"

#include <drivers/vga.h>
#include <drivers/pci.h>
#include <mm/page_table.h>

namespace Kernel
{

Cmd::Cmd()
    : Task(nullptr)
    , Shutdown(false)
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
             Stdlib::StrCmp(cmd, "quit") == 0 ||
             Stdlib::StrCmp(cmd, "shutdown") == 0)
    {
        Shutdown = true;
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
    else if (Stdlib::StrCmp(cmd, "memusage") == 0)
    {
        auto& pt = Mm::PageTable::GetInstance();

        vga.Printf("freePages: %u\n", pt.GetFreePagesCount());
        vga.Printf("totalPages: %u\n", pt.GetTotalPagesCount());
    }
    else if (Stdlib::StrCmp(cmd, "pci") == 0)
    {
        Pci::GetInstance().Dump(vga);
    }
    else if (Stdlib::StrCmp(cmd, "help") == 0)
    {
        vga.Printf("cls - clear screen\n");
        vga.Printf("cpu - dump cpu state\n");
        vga.Printf("dmesg - dump kernel log\n");
        vga.Printf("exit - shutdown kernel\n");
        vga.Printf("ps - show tasks\n");
        vga.Printf("watchdog - show watchdog stats\n");
        vga.Printf("memusage - show memory usage stats\n");
        vga.Printf("pci - show pci devices\n");
        vga.Printf("help - help\n");
    }
    else
    {
        vga.Printf("command '%s' not found\n", cmd);
    }
    vga.Printf("$");
}

bool Cmd::ShouldShutdown()
{
    return Shutdown;
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
        KeyEvent keyEvent = {};
        bool hasEvent = false;
        bool backspace = false;
        {
            Stdlib::AutoLock lock(Lock);
            if (!Buf.IsEmpty())
            {
                keyEvent = Buf.Get();
                hasEvent = true;
                backspace = (keyEvent.Code == 0xE) ? true : false;
            }
        }

        if (hasEvent)
        {
            if (backspace)
            {
                if (pos > 0)
                    vga.Backspace();
            }
            else
            {
                vga.Printf("%c", keyEvent.Char);
            }

            if (keyEvent.Char == '\n')
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
                {
                    if (backspace)
                    {
                        if (pos > 0)
                        {
                            pos--;
                            CmdLine[pos] = '\0';
                        }
                    }
                    else
                    {
                        CmdLine[pos++] = keyEvent.Char;
                    }
                }
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

void Cmd::OnChar(char c, u8 code)
{
    Stdlib::AutoLock lock(Lock);
    if (!Active)
        return;

    KeyEvent e;
    e.Char = c;
    e.Code = code;

    if (!Buf.Put(e))
    {
        Trace(0, "Can't save char");
        return;
    }
}

}