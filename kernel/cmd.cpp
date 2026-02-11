#include "cmd.h"
#include "trace.h"
#include "asm.h"
#include "dmesg.h"
#include "cpu.h"
#include "time.h"
#include "watchdog.h"
#include "block_device.h"
#include "net_device.h"
#include "console.h"

#include <drivers/vga.h>
#include <drivers/pci.h>
#include <drivers/virtio_net.h>
#include <mm/page_table.h>
#include <mm/new.h>

namespace Kernel
{

Cmd::Cmd()
    : TaskPtr(nullptr)
    , Shutdown(false)
    , Reboot(false)
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
    else if (Stdlib::StrCmp(cmd, "poweroff") == 0 ||
             Stdlib::StrCmp(cmd, "shutdown") == 0)
    {
        Shutdown = true;
        return;
    }
    else if (Stdlib::StrCmp(cmd, "reboot") == 0)
    {
        Reboot = true;
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
    else if (Stdlib::StrCmp(cmd, "net") == 0)
    {
        NetDeviceTable::GetInstance().Dump(con);
    }
    else if (Stdlib::MemCmp(cmd, "udpsend ", 8) == 0)
    {
        const char* args = cmd + 8;
        const char* end;
        const char* ipStart = Stdlib::NextToken(args, end);
        if (!ipStart)
        {
            con.Printf("usage: udpsend <ip> <port> <message>\n");
        }
        else
        {
            char ipBuf[16];
            Stdlib::TokenCopy(ipStart, end, ipBuf, sizeof(ipBuf));

            const char* portStart = Stdlib::NextToken(end, end);
            if (!portStart)
            {
                con.Printf("usage: udpsend <ip> <port> <message>\n");
            }
            else
            {
                char portBuf[8];
                Stdlib::TokenCopy(portStart, end, portBuf, sizeof(portBuf));

                ulong port = 0;
                if (!Stdlib::ParseUlong(portBuf, port) || port > 65535)
                {
                    con.Printf("invalid port\n");
                }
                else
                {
                    /* Skip whitespace to get message */
                    const char* msg = end;
                    while (*msg == ' ')
                        msg++;

                    if (*msg == '\0')
                    {
                        con.Printf("usage: udpsend <ip> <port> <message>\n");
                    }
                    else
                    {
                        /* Parse IP: a.b.c.d */
                        u32 dstIp = 0;
                        bool ipOk = true;
                        ulong octet = 0;
                        ulong shift = 24;
                        const char* p = ipBuf;
                        ulong dots = 0;

                        while (*p && ipOk)
                        {
                            if (*p == '.')
                            {
                                if (octet > 255) { ipOk = false; break; }
                                dstIp |= (octet << shift);
                                if (shift == 0) { ipOk = false; break; }
                                shift -= 8;
                                octet = 0;
                                dots++;
                            }
                            else if (*p >= '0' && *p <= '9')
                            {
                                octet = octet * 10 + (*p - '0');
                            }
                            else
                            {
                                ipOk = false;
                            }
                            p++;
                        }

                        if (ipOk && dots == 3 && octet <= 255)
                        {
                            dstIp |= (octet << shift);
                        }
                        else
                        {
                            ipOk = false;
                        }

                        if (!ipOk)
                        {
                            con.Printf("invalid IP '%s'\n", ipBuf);
                        }
                        else
                        {
                            /* Find first net device */
                            NetDevice* dev = nullptr;
                            if (NetDeviceTable::GetInstance().GetCount() > 0)
                                dev = NetDeviceTable::GetInstance().Find("eth0");

                            if (!dev)
                            {
                                con.Printf("no network device\n");
                            }
                            else
                            {
                                VirtioNet* vnet = (VirtioNet*)dev;
                                ulong msgLen = Stdlib::StrLen(msg);
                                u32 srcIp = (10 << 24) | (0 << 16) | (2 << 8) | 15;

                                if (vnet->SendUdp(dstIp, (u16)port,
                                    srcIp, 12345, msg, msgLen))
                                {
                                    con.Printf("sent %u bytes to %s:%u\n",
                                        msgLen, ipBuf, port);
                                }
                                else
                                {
                                    con.Printf("send failed\n");
                                }
                            }
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
        con.Printf("poweroff - power off (ACPI S5)\n");
        con.Printf("reboot - reset system\n");
        con.Printf("ps - show tasks\n");
        con.Printf("watchdog - show watchdog stats\n");
        con.Printf("memusage - show memory usage stats\n");
        con.Printf("pci - show pci devices\n");
        con.Printf("disks - list block devices\n");
        con.Printf("diskread <disk> <sector> - read sector\n");
        con.Printf("diskwrite <disk> <sector> <hex> - write sector\n");
        con.Printf("net - list network devices\n");
        con.Printf("udpsend <ip> <port> <msg> - send UDP packet\n");
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

bool Cmd::ShouldReboot()
{
    return Reboot;
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