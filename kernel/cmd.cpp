#include "cmd.h"
#include "trace.h"
#include "asm.h"
#include "dmesg.h"
#include "cpu.h"
#include "time.h"
#include "watchdog.h"
#include "block_device.h"
#include "console.h"

#include <drivers/vga.h>
#include <drivers/pci.h>
#include <mm/page_table.h>
#include <mm/new.h>

namespace Kernel
{

Cmd::Cmd()
    : TaskPtr(nullptr)
    , Shutdown(false)
    , Active(false)
{
    CmdLine[0] = '\0';
}

Cmd::~Cmd()
{
    if (TaskPtr != nullptr)
    {
        TaskPtr->Put();
        TaskPtr = nullptr;
    }
}

void Cmd::ProcessCmd(const char *cmd)
{
    auto& con = Console::GetInstance();

    if (Stdlib::StrCmp(cmd, "cls") == 0)
    {
        con.Cls();
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
        con.Printf("ss 0x%p cs 0x%p ds 0x%p gs 0x%p fs 0x%p es 0x%p",
            (ulong)GetSs(), (ulong)GetCs(), (ulong)GetDs(),
            (ulong)GetGs(), (ulong)GetFs(), (ulong)GetEs());

        con.Printf("rflags 0x%p rsp 0x%p rip 0x%p\n",
            GetRflags(), GetRsp(), GetRip());

        con.Printf("cr0 0x%p cr2 0x%p cr3 0x%p cr4 0x%p",
            GetCr0(), GetCr2(), GetCr3(), GetCr4());
    }
    else if (Stdlib::StrCmp(cmd, "dmesg") == 0)
    {
        Dmesg::GetInstance().Dump(con);
    }
    else if (Stdlib::StrCmp(cmd, "uptime") == 0)
    {
        auto time = GetBootTime();
        con.Printf("%u.%u\n", time.GetSecs(), time.GetUsecs());
    }
    else if (Stdlib::StrCmp(cmd, "ps") == 0)
    {
        TaskTable::GetInstance().Ps(con);
    }
    else if (Stdlib::StrCmp(cmd, "watchdog") == 0)
    {
        Watchdog::GetInstance().Dump(con);
    }
    else if (Stdlib::StrCmp(cmd, "memusage") == 0)
    {
        auto& pt = Mm::PageTable::GetInstance();

        con.Printf("freePages: %u\n", pt.GetFreePagesCount());
        con.Printf("totalPages: %u\n", pt.GetTotalPagesCount());
    }
    else if (Stdlib::StrCmp(cmd, "pci") == 0)
    {
        Pci::GetInstance().Dump(con);
    }
    else if (Stdlib::StrCmp(cmd, "disks") == 0)
    {
        BlockDeviceTable::GetInstance().Dump(con);
    }
    else if (Stdlib::MemCmp(cmd, "diskread ", 9) == 0)
    {
        const char* args = cmd + 9;
        const char* end;
        const char* nameStart = Stdlib::NextToken(args, end);
        if (!nameStart)
        {
            con.Printf("usage: diskread <disk> <sector>\n");
        }
        else
        {
            char diskName[16];
            Stdlib::TokenCopy(nameStart, end, diskName, sizeof(diskName));

            const char* secStart = Stdlib::NextToken(end, end);
            ulong sector = 0;
            if (!secStart)
            {
                con.Printf("usage: diskread <disk> <sector>\n");
            }
            else
            {
                char secBuf[32];
                Stdlib::TokenCopy(secStart, end, secBuf, sizeof(secBuf));

                if (!Stdlib::ParseUlong(secBuf, sector))
                {
                    con.Printf("invalid sector number\n");
                }
                else
                {
                    BlockDevice* dev = BlockDeviceTable::GetInstance().Find(diskName);
                    if (!dev)
                    {
                        con.Printf("disk '%s' not found\n", diskName);
                    }
                    else
                    {
                        u8 buf[512];
                        if (!dev->ReadSector(sector, buf))
                        {
                            con.Printf("read error\n");
                        }
                        else
                        {
                            for (ulong i = 0; i < 512; i += 16)
                            {
                                con.Printf("%p: ", sector * 512 + i);
                                for (ulong j = 0; j < 16 && (i + j) < 512; j++)
                                {
                                    con.Printf("%p ", (ulong)buf[i + j]);
                                }
                                con.Printf("\n");
                            }
                        }
                    }
                }
            }
        }
    }
    else if (Stdlib::MemCmp(cmd, "diskwrite ", 10) == 0)
    {
        const char* args = cmd + 10;
        const char* end;
        const char* nameStart = Stdlib::NextToken(args, end);
        if (!nameStart)
        {
            con.Printf("usage: diskwrite <disk> <sector> <hex>\n");
        }
        else
        {
            char diskName[16];
            Stdlib::TokenCopy(nameStart, end, diskName, sizeof(diskName));

            const char* secStart = Stdlib::NextToken(end, end);
            ulong sector = 0;
            if (!secStart)
            {
                con.Printf("usage: diskwrite <disk> <sector> <hex>\n");
            }
            else
            {
                char secBuf[32];
                Stdlib::TokenCopy(secStart, end, secBuf, sizeof(secBuf));

                const char* hexStart = Stdlib::NextToken(end, end);
                if (!Stdlib::ParseUlong(secBuf, sector) || !hexStart)
                {
                    con.Printf("usage: diskwrite <disk> <sector> <hex>\n");
                }
                else
                {
                    BlockDevice* dev = BlockDeviceTable::GetInstance().Find(diskName);
                    if (!dev)
                    {
                        con.Printf("disk '%s' not found\n", diskName);
                    }
                    else
                    {
                        u8 buf[512];
                        Stdlib::MemSet(buf, 0, 512);
                        ulong hexLen = (ulong)(end - hexStart);
                        ulong byteCount = 0;
                        if (!Stdlib::HexDecode(hexStart, hexLen, buf, 512, byteCount))
                        {
                            con.Printf("invalid hex data\n");
                        }
                        else if (byteCount > 0)
                        {
                            if (!dev->WriteSector(sector, buf))
                                con.Printf("write error\n");
                            else
                                con.Printf("wrote %u bytes to sector %u\n", byteCount, sector);
                        }
                    }
                }
            }
        }
    }
    else if (Stdlib::StrCmp(cmd, "help") == 0)
    {
        con.Printf("cls - clear screen\n");
        con.Printf("cpu - dump cpu state\n");
        con.Printf("dmesg - dump kernel log\n");
        con.Printf("exit - shutdown kernel\n");
        con.Printf("ps - show tasks\n");
        con.Printf("watchdog - show watchdog stats\n");
        con.Printf("memusage - show memory usage stats\n");
        con.Printf("pci - show pci devices\n");
        con.Printf("disks - list block devices\n");
        con.Printf("diskread <disk> <sector> - read sector\n");
        con.Printf("diskwrite <disk> <sector> <hex> - write sector\n");
        con.Printf("help - help\n");
    }
    else
    {
        con.Printf("command '%s' not found\n", cmd);
    }
    con.Printf("$");
}

bool Cmd::ShouldShutdown()
{
    return Shutdown;
}

void Cmd::Stop()
{
    if (Active)
    {
        BugOn(TaskPtr == nullptr);
        Active = false;
        TaskPtr->SetStopping();
        TaskPtr->Wait();
    }
}

bool Cmd::Start()
{
    if (TaskPtr != nullptr)
        return false;

    auto task = Mm::TAlloc<Task, Tag>("cmd");
    if (task == nullptr)
        return false;

    {
        Stdlib::AutoLock lock(Lock);
        if (TaskPtr == nullptr)
        {
            TaskPtr = task;
        }
    }

    if (TaskPtr != task)
    {
        task->Put();
        return false;
    }

    if (!TaskPtr->Start(&Cmd::RunFunc, this))
    {
        {
            Stdlib::AutoLock lock(Lock);
            task = TaskPtr;
            TaskPtr = nullptr;
        }
        task->Put();
        return false;
    }

    {
        Stdlib::AutoLock lock(Lock);
        Active = true;
    }
    return true;
}

void Cmd::ShowBanner(Stdlib::Printer& out)
{
    out.Printf("\n");
    out.Printf("  _   _  ___  ____\n");
    out.Printf(" | \\ | |/ _ \\/ ___|\n");
    out.Printf(" |  \\| | | | \\___ \\\n");
    out.Printf(" | |\\  | |_| |___) |\n");
    out.Printf(" |_| \\_|\\___/|____/\n");
    out.Printf("\n$");
}

void Cmd::Run()
{
    size_t pos = 0;
    bool overflow = false;

    auto& con = Console::GetInstance();

    /* Wait for startup trace output to settle before showing banner */
    Sleep(100 * Const::NanoSecsInMs);
    ShowBanner(con);

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
                    con.Backspace();
            }
            else
            {
                con.Printf("%c", keyEvent.Char);
            }

            if (keyEvent.Char == '\n')
            {
                CmdLine[pos] = '\0';
                if (!overflow)
                {
                    ProcessCmd(CmdLine);
                }
                else
                {
                    con.Printf("command too large\n");
                    con.Printf("$");
                    overflow = false;
                }
                Stdlib::MemSet(CmdLine, 0, sizeof(CmdLine));
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