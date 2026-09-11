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

    /* Take what can be taken and name what cannot. A parameter that cannot be
       parsed is logged and skipped and the rest of the line still read, as
       Linux does, and a line longer than CmdlineLen - 1 characters is read
       up to its last whole parameter. Nothing here stops the boot: this runs
       before the netconsole, the disk log or the NIC exist, so on a machine
       whose only console is the network a boot that stopped here would stop
       without a word. */
    void Parse(const char *cmdline);

    /* Whether Parse() has run. Before it every getter reports its default,
       and a caller that must not act on a default -- the disk log, keeping
       the first lines of the boot until it knows -- can tell. */
    bool IsParsed();

    /* The line, its terminator included, and the longest single parameter
       taken -- room for root=UUID=<uuid>, which is 46 */
    static const ulong CmdlineLen = 256;
    static const ulong MaxParamLen = 48;

    bool IsTraceVga();
    bool IsPanicVga();
    bool IsSmpOff();

    /* maxcpus=N -- bring up at most N CPUs, the BSP included. 0 = no cap. */
    ulong GetMaxCpus();

    bool IsItsEnabled();
    bool IsWxProbe();
    bool IsUsbOff();

    /* hwrng=off -- ignore the cpu's random instruction (RDRAND/RDSEED,
       RNDR). For seeing what a machine without one does, on a machine that
       has one; the pool then falls back on virtio-rng and timing jitter. */
    bool IsHwRngOff();

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

    /* rxpoll=on -- have the tick look at the receive path as well as the
       NIC's own interrupt. Off by default: it is a hypothesis about a driver
       stall, not a proven fix, and on the machine it was meant to help the
       stall arrived sooner with it on than with it off. Left behind a switch
       so the two can be compared without a rebuild, which is the only way to
       tell on a box whose console is a UDP socket. */
    bool IsRxPollEnabled();

    /* loglevel=N: the trace level to boot with. Defaults to what main sets
       today; the `loglevel` shell command moves it afterwards. */
    int GetLogLevel();
    static const int DefaultLogLevel = 1;

    bool IsDnsEnabled();

    /* root=auto | <device> | LABEL=<label> | UUID=<uuid>: what to mount on /
       (see fs/rootfs.cpp). Value holds the device name or label; Uuid the
       parsed UUID. */
    enum RootMode {
        RootNone = 0,
        RootAuto,
        RootDevice,
        RootLabel,
        RootUuid,
    };

    static const ulong RootValueLen = 40;
    static const ulong UuidBytes = 16;
    static const ulong UuidTextLen = 36;

    struct RootSpec {
        RootMode Mode;
        char Value[RootValueLen];
        u8 Uuid[UuidBytes];
    };

    const RootSpec& GetRoot();

    /* ro: mount the root filesystem read-only */
    bool IsRootReadOnly();

    /* fstest=on: run the filesystem self-test on / once it is mounted */
    bool IsFsTest();

    /* disklog=on: write the kernel log to a disk area prepared with
       scripts/disklog.py (see kernel/disklog.h). Off by default: it costs
       a forced disk write per burst of lines. */
    bool IsDiskLogOn();

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

    char Cmdline[CmdlineLen];
    bool TraceVga;
    bool PanicVga;
    bool SmpOff;
    ulong MaxCpusLimit;
    bool ItsEnabled;
    bool HwRngOff;
    bool WxProbe;
    bool UsbOff;
    ConsoleMode ConMode;
    DhcpMode DhcpMd;
    u16 UdpShellPort;
    Net::IpAddress NetconsoleIp;
    u16 NetconsolePort;
    ulong NetconsoleTailKb;
    ulong NetFrameCount;
    bool RxPoll;
    int LogLevel;
    bool DnsEnabled;
    RootSpec Root;
    bool RootReadOnly;
    bool FsTest;
    bool DiskLogOn;
    bool Parsed;

    static bool ParseUuid(const char* text, u8* out);
};
}