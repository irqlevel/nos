#pragma once

#include <lib/stdlib.h>
#include <net/net.h>

namespace Kernel
{

class Parameters
{
public:
    static Parameters& GetInstance()
    {
        static Parameters Instance;
        return Instance;
    }

    bool Parse(const char *cmdline);

    bool IsTraceVga();
    bool IsPanicVga();
    bool IsSmpOff();

    /* maxcpus=N -- bring up at most N CPUs, the BSP included. 0 = no cap. */
    ulong GetMaxCpus();

    bool IsItsEnabled();
    bool IsWxProbe();
    bool IsUsbOff();

    bool IsConsoleSerial();
    bool IsConsoleVga();
    bool IsConsoleBoth();

    bool IsDhcpAuto();
    bool IsDhcpOff();

    u16 GetUdpShellPort();

    /* netconsole=ip:port -- stream the kernel log to that collector */
    bool IsNetconsoleEnabled();
    Net::IpAddress GetNetconsoleIp();
    u16 GetNetconsolePort();

    /* nctail=N -- when the link comes up, ship only the newest N KiB of the
       buffered boot log. 0 (the default) ships all of it. */
    ulong GetNetconsoleTailKb();

    /* netframes=N -- how many frames the network pool is built with. 0 (the
       default) means NetFramePool::DefaultFrameCount. Worth having as a knob
       rather than a rebuild: how many frames a driver keeps in flight is a
       property of that driver and that load, and the only way to find out is
       to raise it on the machine in question and watch `netpool` for misses. */
    ulong GetNetFrameCount();

    /* loglevel=N: the trace level to boot with. Defaults to what main sets
       today; the `loglevel` shell command moves it afterwards. */
    int GetLogLevel();
    static const int DefaultLogLevel = 1;

    bool IsDnsEnabled();

    bool IsRootAuto();

    const char* GetCmdline();

    Parameters();
    ~Parameters();
private:
    bool ParseParameter(const char *cmdline, size_t start, size_t end);

    enum ConsoleMode {
        ConsoleBoth = 0,
        ConsoleSerialOnly,
        ConsoleVgaOnly,
    };

    enum DhcpMode {
        DhcpOn = 0,    /* start only by cmd (default) */
        DhcpAuto,      /* start automatically at boot */
        DhcpOff,       /* disabled entirely */
    };

    char Cmdline[256];
    bool TraceVga;
    bool PanicVga;
    bool SmpOff;
    ulong MaxCpusLimit;
    bool ItsEnabled;
    bool WxProbe;
    bool UsbOff;
    ConsoleMode ConMode;
    DhcpMode DhcpMd;
    u16 UdpShellPort;
    Net::IpAddress NetconsoleIp;
    u16 NetconsolePort;
    ulong NetconsoleTailKb;
    ulong NetFrameCount;
    int LogLevel;
    bool DnsEnabled;
    bool RootAuto;
};
}