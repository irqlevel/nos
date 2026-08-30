#include "parameters.h"
#include "cpu.h"
#include "panic.h"
#include "trace.h"

namespace Kernel
{

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
    , WxProbe(false)
    , UsbOff(false)
    , ConMode(ConsoleBoth)
    , DhcpMd(DhcpOn)
    , UdpShellPort(0)
    , NetconsolePort(0)
    , NetconsoleTailKb(0)
    , DnsEnabled(false)
    , RootAuto(false)
{
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

bool Parameters::IsDnsEnabled()
{
    return DnsEnabled;
}

bool Parameters::IsRootAuto()
{
    return RootAuto;
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

    if (len < 3)
        return false;

    Stdlib::StrnCpy(param, &cmdline[start], len + 1);
    
    const char* sep = Stdlib::StrChrOnce(param, '=');
    if (sep == nullptr)
        return false;

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
    else if (Stdlib::StrCmp(key, "root") == 0)
    {
        if (Stdlib::StrCmp(value, "auto") == 0)
        {
            RootAuto = true;
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
