#include "parameters.h"
#include "cpu.h"
#include "panic.h"
#include "trace.h"

namespace Kernel
{

/* Bounds on netframes=N. The floor keeps a typo from starving the datapath;
   the ceiling keeps one from reserving a quarter of a gigabyte at boot, since
   the pool is built once and never grows. */
static const ulong NetFrameCountMin = 64;
static const ulong NetFrameCountMax = 65536;

/* Upper bound on nctail=N. The netconsole ring is smaller than this, and it
   treats a cap at or above its own size as no cap at all -- this only keeps
   a typo from being taken for a number. */
static const ulong NetconsoleTailKbMax = 1024;

Parameters::Parameters()
    : TraceVga(false)
    , PanicVga(false)
    , SmpOff(false)
    , MaxCpusLimit(0)
    , ItsEnabled(true)  /* PCIe MSI via GICv3 ITS is on by default; its=off to disable */
    , HwRngOff(false)
    , WxProbe(false)
    , UsbOff(false)
    , ConMode(ConsoleBoth)
    , DhcpMd(DhcpOn)
    , UdpShellPort(0)
    , NetconsolePort(0)
    , NetconsoleTailKb(0)
    , NetFrameCount(0)
    , RxPoll(false)
    , LogLevel(DefaultLogLevel)
    , DnsEnabled(false)
    , RootReadOnly(false)
    , FsTest(false)
{
    Stdlib::MemSet(&Root, 0, sizeof(Root));
    Root.Mode = RootNone;
}

Parameters::~Parameters()
{
}

bool Parameters::IsTraceVga()
{
    return TraceVga;
}

bool Parameters::IsPanicVga()
{
    return PanicVga;
}

bool Parameters::IsSmpOff()
{
    return SmpOff;
}

ulong Parameters::GetMaxCpus()
{
    return MaxCpusLimit;
}

bool Parameters::IsHwRngOff()
{
    return HwRngOff;
}

bool Parameters::IsItsEnabled()
{
    return ItsEnabled;
}

bool Parameters::IsWxProbe()
{
    return WxProbe;
}

bool Parameters::IsUsbOff()
{
    return UsbOff;
}

bool Parameters::IsConsoleSerial()
{
    return ConMode == ConsoleSerialOnly;
}

bool Parameters::IsConsoleVga()
{
    return ConMode == ConsoleVgaOnly;
}

bool Parameters::IsConsoleBoth()
{
    return ConMode == ConsoleBoth;
}

bool Parameters::IsDhcpAuto()
{
    return DhcpMd == DhcpAuto;
}

bool Parameters::IsDhcpOff()
{
    return DhcpMd == DhcpOff;
}

u16 Parameters::GetUdpShellPort()
{
    return UdpShellPort;
}

bool Parameters::IsNetconsoleEnabled()
{
    return NetconsolePort != 0;
}

Net::IpAddress Parameters::GetNetconsoleIp()
{
    return NetconsoleIp;
}

u16 Parameters::GetNetconsolePort()
{
    return NetconsolePort;
}

ulong Parameters::GetNetconsoleTailKb()
{
    return NetconsoleTailKb;
}

ulong Parameters::GetNetFrameCount()
{
    return NetFrameCount;
}

bool Parameters::IsRxPollEnabled()
{
    return RxPoll;
}

int Parameters::GetLogLevel()
{
    return LogLevel;
}

bool Parameters::IsDnsEnabled()
{
    return DnsEnabled;
}

const Parameters::RootSpec& Parameters::GetRoot()
{
    return Root;
}

bool Parameters::IsRootReadOnly()
{
    return RootReadOnly;
}

bool Parameters::IsFsTest()
{
    return FsTest;
}

/* xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx, as blkid prints it */
bool Parameters::ParseUuid(const char* text, u8* out)
{
    if (Stdlib::StrLen(text) != UuidTextLen)
        return false;

    char hex[UuidBytes * 2 + 1];
    ulong n = 0;
    for (ulong i = 0; i < UuidTextLen; i++)
    {
        if (i == 8 || i == 13 || i == 18 || i == 23)
        {
            if (text[i] != '-')
                return false;
            continue;
        }
        hex[n++] = text[i];
    }
    hex[n] = '\0';

    ulong bytes = 0;
    return Stdlib::HexDecode(hex, n, out, UuidBytes, bytes) && bytes == UuidBytes;
}

const char* Parameters::GetCmdline()
{
    return Cmdline;
}

bool Parameters::ParseParameter(const char *cmdline, size_t start, size_t end)
{
    if (BugOn(start >= end))
        return false;

    /* Long enough for the widest value we take: netconsole=255.255.255.255:65535 */
    const size_t maxLen = 48;
    char param[maxLen + 1];
    size_t len = end - start;
    if (len > maxLen)
        return false;

    /* The shortest token taken is the bare word ro */
    if (len < 2)
        return false;

    Stdlib::StrnCpy(param, &cmdline[start], len + 1);

    /* The key ends at the first '=': the value may hold one of its own, as
       root=LABEL=<label> and root=UUID=<uuid> do */
    const char* sep = Stdlib::StrChr(param, '=');
    if (sep == nullptr)
    {
        /* The one bare word taken, as on Linux: ro */
        if (Stdlib::StrCmp(param, "ro") == 0)
        {
            Trace(0, "Key %s", param);
            RootReadOnly = true;
            return true;
        }
        return false;
    }

    if ((sep == param) || (sep == &param[len - 1]))
        return false;

    size_t keyLen = sep - param;
    const char *key = &param[0];
    param[keyLen] = '\0';
    const char *value = &param[keyLen + 1];

    Trace(0, "Key %s value %s", key, value);

    if (Stdlib::StrCmp(key, "trace") == 0)
    {
        if (Stdlib::StrCmp(value, "vga") == 0)
        {
            TraceVga = true;
        }
        else
        {
            Trace(0, "Unknown value %s, key %s", value, key);
        }
    }
    else if (Stdlib::StrCmp(key, "panic") == 0)
    {
        if (Stdlib::StrCmp(value, "vga") == 0)
        {
            PanicVga = true;
        }
        else
        {
            Trace(0, "Unknown value %s, key %s", value, key);
        }
    }
    else if (Stdlib::StrCmp(key, "its") == 0)
    {
        ItsEnabled = (Stdlib::StrCmp(value, "off") != 0);
    }
    else if (Stdlib::StrCmp(key, "usb") == 0)
    {
        UsbOff = (Stdlib::StrCmp(value, "off") == 0);
    }
    else if (Stdlib::StrCmp(key, "hwrng") == 0)
    {
        HwRngOff = (Stdlib::StrCmp(value, "off") == 0);
    }
    else if (Stdlib::StrCmp(key, "wxprobe") == 0)
    {
        WxProbe = (Stdlib::StrCmp(value, "on") == 0);
    }
    else if (Stdlib::StrCmp(key, "smp") == 0)
    {
        if (Stdlib::StrCmp(value, "off") == 0)
        {
            SmpOff = true;
        }
        else
        {
            Trace(0, "Unknown value %s, key %s", value, key);
        }
    }
    else if (Stdlib::StrCmp(key, "maxcpus") == 0)
    {
        ulong count = 0;
        if (Stdlib::ParseUlong(value, count) && count > 0 && count <= MaxCpus)
        {
            MaxCpusLimit = count;
        }
        else
        {
            Trace(0, "Invalid maxcpus %s", value);
        }
    }
    else if (Stdlib::StrCmp(key, "console") == 0)
    {
        if (Stdlib::StrCmp(value, "serial") == 0)
        {
            ConMode = ConsoleSerialOnly;
        }
        else if (Stdlib::StrCmp(value, "vga") == 0)
        {
            ConMode = ConsoleVgaOnly;
        }
        else if (Stdlib::StrCmp(value, "both") == 0)
        {
            ConMode = ConsoleBoth;
        }
        else
        {
            Trace(0, "Unknown value %s, key %s", value, key);
        }
    }
    else if (Stdlib::StrCmp(key, "dhcp") == 0)
    {
        if (Stdlib::StrCmp(value, "auto") == 0)
        {
            DhcpMd = DhcpAuto;
        }
        else if (Stdlib::StrCmp(value, "on") == 0)
        {
            DhcpMd = DhcpOn;
        }
        else if (Stdlib::StrCmp(value, "off") == 0)
        {
            DhcpMd = DhcpOff;
        }
        else
        {
            Trace(0, "Unknown value %s, key %s", value, key);
        }
    }
    else if (Stdlib::StrCmp(key, "udpshell") == 0)
    {
        ulong port = 0;
        if (Stdlib::ParseUlong(value, port) && port > 0 && port <= 65535)
        {
            UdpShellPort = (u16)port;
        }
        else
        {
            Trace(0, "Invalid udpshell port %s", value);
        }
    }
    else if (Stdlib::StrCmp(key, "netconsole") == 0)
    {
        /* netconsole=ip:port */
        const char* colon = Stdlib::StrChrOnce(value, ':');
        if (colon == nullptr || colon == value || *(colon + 1) == '\0')
        {
            Trace(0, "Invalid netconsole value %s, expected ip:port", value);
        }
        else
        {
            char ipBuf[16];
            size_t ipLen = colon - value;
            ulong port = 0;

            Net::IpAddress ip;
            if (ipLen >= sizeof(ipBuf))
            {
                Trace(0, "Invalid netconsole ip in %s", value);
            }
            else
            {
                Stdlib::StrnCpy(ipBuf, value, ipLen + 1);
                if (!Net::IpAddress::Parse(ipBuf, ip))
                {
                    Trace(0, "Invalid netconsole ip %s", ipBuf);
                }
                else if (!Stdlib::ParseUlong(colon + 1, port) || port == 0 || port > 65535)
                {
                    Trace(0, "Invalid netconsole port %s", colon + 1);
                }
                else
                {
                    NetconsoleIp = ip;
                    NetconsolePort = (u16)port;
                }
            }
        }
    }
    else if (Stdlib::StrCmp(key, "rxpoll") == 0)
    {
        RxPoll = (Stdlib::StrCmp(value, "on") == 0);
    }
    else if (Stdlib::StrCmp(key, "netframes") == 0)
    {
        ulong count = 0;
        if (Stdlib::ParseUlong(value, count) &&
            count >= NetFrameCountMin && count <= NetFrameCountMax)
        {
            NetFrameCount = count;
        }
        else
        {
            Trace(0, "Invalid netframes %s", value);
        }
    }
    else if (Stdlib::StrCmp(key, "nctail") == 0)
    {
        ulong kb = 0;
        if (Stdlib::ParseUlong(value, kb) && kb > 0 && kb <= NetconsoleTailKbMax)
        {
            NetconsoleTailKb = kb;
        }
        else
        {
            Trace(0, "Invalid nctail %s", value);
        }
    }
    else if (Stdlib::StrCmp(key, "loglevel") == 0)
    {
        /* The runtime `loglevel` shell command cannot help with a boot that
           has already happened, and the chattier levels are exactly the ones
           worth having during bring-up on a machine you cannot log into. */
        ulong level = 0;
        if (Stdlib::ParseUlong(value, level) && level <= (ulong)MaxTraceLevel)
        {
            LogLevel = (int)level;
        }
        else
        {
            Trace(0, "Invalid loglevel %s", value);
        }
    }
    else if (Stdlib::StrCmp(key, "root") == 0)
    {
        const char* labelPrefix = "LABEL=";
        const char* uuidPrefix = "UUID=";
        ulong labelPrefixLen = Stdlib::StrLen(labelPrefix);
        ulong uuidPrefixLen = Stdlib::StrLen(uuidPrefix);

        Stdlib::MemSet(&Root, 0, sizeof(Root));
        if (Stdlib::StrCmp(value, "auto") == 0)
        {
            Root.Mode = RootAuto;
        }
        else if (Stdlib::StrnCmp(value, labelPrefix, labelPrefixLen) == 0 &&
                 value[labelPrefixLen] != '\0')
        {
            Root.Mode = RootLabel;
            Stdlib::StrnCpy(Root.Value, value + labelPrefixLen, sizeof(Root.Value));
        }
        else if (Stdlib::StrnCmp(value, uuidPrefix, uuidPrefixLen) == 0)
        {
            if (ParseUuid(value + uuidPrefixLen, Root.Uuid))
            {
                Root.Mode = RootUuid;
                Stdlib::StrnCpy(Root.Value, value + uuidPrefixLen, sizeof(Root.Value));
            }
            else
            {
                Trace(0, "Invalid UUID %s, key %s", value, key);
            }
        }
        else
        {
            Root.Mode = RootDevice;
            Stdlib::StrnCpy(Root.Value, value, sizeof(Root.Value));
        }
    }
    else if (Stdlib::StrCmp(key, "fstest") == 0)
    {
        if (Stdlib::StrCmp(value, "on") == 0)
        {
            FsTest = true;
        }
        else if (Stdlib::StrCmp(value, "off") == 0)
        {
            FsTest = false;
        }
        else
        {
            Trace(0, "Unknown value %s, key %s", value, key);
        }
    }
    else if (Stdlib::StrCmp(key, "dns") == 0)
    {
        if (Stdlib::StrCmp(value, "on") == 0)
        {
            DnsEnabled = true;
        }
        else
        {
            Trace(0, "Unknown value %s, key %s", value, key);
        }
    }
    else
    {
        Trace(0, "Unknown key %s, skipping", key);
    }

    return true;
}

bool Parameters::Parse(const char *cmdline)
{
    if (Stdlib::SnPrintf(Cmdline, Stdlib::ArraySize(Cmdline), "%s", cmdline) < 0)
        return false;

    size_t start = 0, i = 0;
    for (; i < Stdlib::StrLen(Cmdline); i++)
    {
        if (Cmdline[i] == ' ')
        {
            if (start < i)
            {
                if (!ParseParameter(Cmdline, start, i))
                    return false;
            }
            start = i + 1;
        }
    }

    if (start < i)
    {
        if (!ParseParameter(Cmdline, start, i))
            return false;
    }

    return true;
}

}
