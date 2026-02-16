#pragma once

#include <lib/stdlib.h>

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

    bool IsConsoleSerial();
    bool IsConsoleVga();
    bool IsConsoleBoth();

    bool IsDhcpAuto();
    bool IsDhcpOff();

    u16 GetUdpShellPort();

    bool IsDnsEnabled();

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
    ConsoleMode ConMode;
    DhcpMode DhcpMd;
    u16 UdpShellPort;
    bool DnsEnabled;
};
}