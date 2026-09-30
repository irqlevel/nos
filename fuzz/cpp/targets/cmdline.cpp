// The kernel command line (kernel/parameters.cpp): what GRUB's config, the
// device tree's bootargs or QEMU's -append hand the kernel before anything
// else runs, and the first thing a boot on a machine with no serial port
// acts on -- the netconsole to report to, the root to mount, the CPUs to
// bring up. A line is built from the input of the parameters
// docs/kernel-parameters.md names, with values right and wrong, of words
// nobody named, of tokens past the longest one taken and of lines past the
// longest one kept, and handed to Parameters::Parse; every getter is held to
// a reader of the line written from parameters.h: tokens split on spaces, a
// parameter longer than MaxParamLen skipped, a line past CmdlineLen - 1 read
// up to its last whole parameter, each key's value taken as its branch says,
// the last word on a key the one that counts.
#include "host.h"

#include <kernel/cpu.h>
#include <kernel/parameters.h>
#include <kernel/trace.h>
#include <kernel/ubsan.h>

#include "fuzz.h"

#include <ctype.h>

#include <new>

using Kernel::Parameters;

namespace Kernel
{
namespace Ubsan
{

/* ubsan=warn: the one parameter that reaches into another subsystem */
bool WarnOnly;

void SetWarnOnly(bool warnOnly)
{
    WarnOnly = warnOnly;
}

}
}

namespace
{

/* ---- the model: parameters.h's reading of a line ---- */

struct Expected
{
    bool TraceVga = false, PanicVga = false, SmpOff = false, ItsEnabled = true, HwRngOff = false;
    int WxProbe = 0; /* off, text, heap */
    bool UsbOff = false, RcOff = false;
    int Console = 0; /* both, serial, vga */
    int Dhcp = 0;    /* on, auto, off */
    ulong MaxCpus = 0, NetFrames = 0, TailKb = 0;
    u16 UdpShell = 0, NcPort = 0;
    u32 NcIp = 0;
    bool RxPoll = false, Dns = false, Ro = false, FsTest = false, DiskLog = false;
    int LogLevel = Parameters::DefaultLogLevel;
    int RootMode = Parameters::RootNone;
    std::string RootValue;
    std::vector<uint8_t> RootUuid = std::vector<uint8_t>(Parameters::UuidBytes, 0);
    bool UbsanWarn = false;
    bool UbsanSet = false;
    std::string Kept;
};

bool Number(const std::string& s, unsigned long long& out)
{
    if (s.empty() || s.size() > 20)
        return false;
    unsigned __int128 v = 0;
    for (char c : s)
    {
        if (c < '0' || c > '9')
            return false;
        v = v * 10 + (c - '0');
    }
    if (v > ~0ULL)
        return false;
    out = static_cast<unsigned long long>(v);
    return true;
}

bool Ip(const std::string& s, u32& out)
{
    std::vector<std::string> parts;
    size_t at = 0;
    for (;;)
    {
        size_t dot = s.find('.', at);
        parts.push_back(s.substr(at, dot == std::string::npos ? std::string::npos : dot - at));
        if (dot == std::string::npos)
            break;
        at = dot + 1;
    }
    if (parts.size() != 4)
        return false;
    u32 addr = 0;
    for (const std::string& p : parts)
    {
        unsigned long long v;
        if (!Number(p, v) || v > 255)
            return false;
        addr = (addr << 8) | static_cast<u32>(v);
    }
    out = addr;
    return true;
}

bool Uuid(const std::string& s, std::vector<uint8_t>& out)
{
    if (s.size() != Parameters::UuidTextLen)
        return false;
    std::string hex;
    for (size_t i = 0; i < s.size(); i++)
    {
        if (i == 8 || i == 13 || i == 18 || i == 23)
        {
            if (s[i] != '-')
                return false;
            continue;
        }
        if (!isxdigit(static_cast<unsigned char>(s[i])))
            return false;
        hex += s[i];
    }
    out.assign(Parameters::UuidBytes, 0);
    for (size_t i = 0; i < Parameters::UuidBytes; i++)
        out[i] = static_cast<uint8_t>(std::stoul(hex.substr(2 * i, 2), nullptr, 16));
    return true;
}

void Take(Expected& e, const std::string& token)
{
    if (token.size() > Parameters::MaxParamLen || token.size() < 2)
        return;
    size_t eq = token.find('=');
    if (eq == std::string::npos)
    {
        if (token == "ro")
            e.Ro = true;
        return;
    }
    if (eq == 0 || eq == token.size() - 1)
        return;
    std::string key = token.substr(0, eq), value = token.substr(eq + 1);
    unsigned long long n;
    if (key == "trace" && value == "vga")
        e.TraceVga = true;
    else if (key == "panic" && value == "vga")
        e.PanicVga = true;
    else if (key == "its")
        e.ItsEnabled = value != "off";
    else if (key == "usb")
        e.UsbOff = value == "off";
    else if (key == "rc")
        e.RcOff = value == "off";
    else if (key == "hwrng")
        e.HwRngOff = value == "off";
    else if (key == "wxprobe")
    {
        if (value == "on" || value == "text")
            e.WxProbe = 1;
        else if (value == "heap")
            e.WxProbe = 2;
    }
    else if (key == "smp" && value == "off")
        e.SmpOff = true;
    else if (key == "maxcpus")
    {
        if (Number(value, n) && n > 0 && n <= Kernel::MaxCpus)
            e.MaxCpus = n;
    }
    else if (key == "console")
    {
        if (value == "serial")
            e.Console = 1;
        else if (value == "vga")
            e.Console = 2;
        else if (value == "both")
            e.Console = 0;
    }
    else if (key == "dhcp")
    {
        if (value == "auto")
            e.Dhcp = 1;
        else if (value == "on")
            e.Dhcp = 0;
        else if (value == "off")
            e.Dhcp = 2;
    }
    else if (key == "udpshell")
    {
        if (Number(value, n) && n > 0 && n <= 65535)
            e.UdpShell = static_cast<u16>(n);
    }
    else if (key == "netconsole")
    {
        /* ip:port -- one colon, something on either side of it */
        size_t colon = value.find(':');
        if (colon == std::string::npos || value.find(':', colon + 1) != std::string::npos || colon == 0 ||
            colon == value.size() - 1)
            return;
        std::string ip = value.substr(0, colon);
        u32 addr;
        if (ip.size() >= 16 || !Ip(ip, addr))
            return;
        if (!Number(value.substr(colon + 1), n) || n == 0 || n > 65535)
            return;
        e.NcIp = addr;
        e.NcPort = static_cast<u16>(n);
    }
    else if (key == "rxpoll")
        e.RxPoll = value == "on";
    else if (key == "ubsan")
    {
        e.UbsanSet = true;
        e.UbsanWarn = value == "warn";
    }
    else if (key == "netframes")
    {
        if (Number(value, n) && n >= 64 && n <= 65536)
            e.NetFrames = n;
    }
    else if (key == "nctail")
    {
        if (Number(value, n) && n > 0 && n <= 1024)
            e.TailKb = n;
    }
    else if (key == "loglevel")
    {
        if (Number(value, n) && n <= static_cast<unsigned long long>(Kernel::MaxTraceLevel))
            e.LogLevel = static_cast<int>(n);
    }
    else if (key == "root")
    {
        /* Whatever it says, what was said before is gone */
        e.RootMode = Parameters::RootNone;
        e.RootValue.clear();
        e.RootUuid.assign(Parameters::UuidBytes, 0);
        if (value == "auto")
        {
            e.RootMode = Parameters::RootAuto;
        }
        else if (value.compare(0, 6, "LABEL=") == 0 && value.size() > 6)
        {
            e.RootMode = Parameters::RootLabel;
            e.RootValue = value.substr(6, Parameters::RootValueLen - 1);
        }
        else if (value.compare(0, 5, "UUID=") == 0)
        {
            if (Uuid(value.substr(5), e.RootUuid))
            {
                e.RootMode = Parameters::RootUuid;
                e.RootValue = value.substr(5, Parameters::RootValueLen - 1);
            }
            else
            {
                e.RootUuid.assign(Parameters::UuidBytes, 0);
            }
        }
        else
        {
            e.RootMode = Parameters::RootDevice;
            e.RootValue = value.substr(0, Parameters::RootValueLen - 1);
        }
    }
    else if (key == "fstest")
    {
        if (value == "on")
            e.FsTest = true;
        else if (value == "off")
            e.FsTest = false;
    }
    else if (key == "disklog")
    {
        if (value == "on")
            e.DiskLog = true;
        else if (value == "off")
            e.DiskLog = false;
    }
    else if (key == "dns" && value == "on")
    {
        e.Dns = true;
    }
}

Expected Read(const std::string& line)
{
    Expected e;
    /* What is kept of the line: CmdlineLen - 1 characters, and if that cuts
       through a parameter, only up to the one before it */
    const size_t keep = Parameters::CmdlineLen - 1;
    std::string kept = line.substr(0, keep);
    if (line.size() > keep && line[keep] != ' ')
    {
        size_t space = kept.rfind(' ');
        kept = (space == std::string::npos) ? std::string() : kept.substr(0, space + 1);
    }
    e.Kept = kept;
    size_t at = 0;
    while (at <= kept.size())
    {
        size_t space = kept.find(' ', at);
        size_t end = (space == std::string::npos) ? kept.size() : space;
        if (end > at)
            Take(e, kept.substr(at, end - at));
        at = end + 1;
    }
    return e;
}

/* ---- the line ---- */

const char* const Words[] = {"trace", "panic", "its", "usb", "rc", "hwrng", "wxprobe", "smp", "maxcpus", "console",
                             "dhcp", "udpshell", "netconsole", "rxpoll", "ubsan", "netframes", "nctail", "loglevel",
                             "root", "fstest", "disklog", "dns"};

const char* const BadIps[] = {"1.2.3", "1.2.3.4.5", "01.2.3.4", "1..2.3", "a.b.c.d", "1.2.3.4:5",
                              "255.255.255.255"};

const char* const Strays[] = {"quiet", "splash", "r", "=", "a=", "=b", "foo=bar", "nokaslr", "console=ttyS0,115200",
                              "ro=1"};

const char* const Values[] = {"on", "off", "vga", "serial", "both", "auto", "text", "heap", "warn", "", "0", "1",
                              "64", "65", "65535", "65536", "1024", "1025", "5", "6", "18446744073709551615",
                              "18446744073709551616", "-1", "1x", "abc"};

std::string Value(Fuzz::Input& in, const std::string& key)
{
    switch (in.U8() % 8)
    {
    case 0:
        return in.Pick(Values);
    case 1:
        return std::to_string(in.Value32());
    default:
        break;
    }
    if (key == "netconsole")
    {
        std::string ip = std::to_string(in.Below(300)) + "." + std::to_string(in.Below(256)) + "." +
                         std::to_string(in.Below(256)) + "." + std::to_string(in.Below(260));
        if (in.Chance(24))
            ip = in.Pick(BadIps);
        return ip + (in.Chance(16) ? "" : ":") + std::to_string(in.Below(70000));
    }
    if (key == "root")
    {
        switch (in.U8() % 5)
        {
        case 0:
            return "auto";
        case 1:
            return "LABEL=" + std::string(in.Below(45), 'l');
        case 2:
        {
            std::string u = "UUID=";
            for (int i = 0; i < 36; i++)
                u += (i == 8 || i == 13 || i == 18 || i == 23) ? '-' : "0123456789abcdefABCDEFg"[in.Below(in.Chance(8) ? 23 : 22)];
            if (in.Chance(16))
                u.pop_back();
            return u;
        }
        default:
            return std::string("vd") + static_cast<char>('a' + in.Below(4)) + (in.Bool() ? "1" : "");
        }
    }
    return in.Pick(Values);
}

std::string Token(Fuzz::Input& in)
{
    switch (in.U8() % 12)
    {
    case 0:
        return "ro";
    case 1:
    {
        /* A word nobody named, a Linux habit */
        return in.Pick(Strays);
    }
    case 2:
        /* Past the longest parameter taken, and at it */
        return "root=" + std::string(in.Range(40, 50), 'x');
    case 3:
    {
        std::string t;
        for (int i = in.Below(12); i > 0; i--)
            t += static_cast<char>(in.Chance(32) ? in.Range(1, 255) : 'a' + in.Below(26));
        return t;
    }
    default:
    {
        std::string key = in.Pick(Words);
        return key + "=" + Value(in, key);
    }
    }
}

void Run(Fuzz::Input& in)
{
    Parameters& p = Parameters::GetInstance();
    if (in.Chance(4))
    {
        p.Parse(nullptr);
        INVARIANT(p.IsParsed(), "no line, and the parameters not parsed");
        return;
    }
    std::string line;
    for (int n = in.Chance(32) ? 40 : in.Below(12); n > 0; n--)
    {
        std::string sep(in.Chance(32) ? in.Range(2, 4) : 1, ' ');
        line += (line.empty() ? std::string(in.Chance(16) ? 1 : 0, ' ') : sep) + Token(in);
    }
    if (in.Chance(8))
        line += ' ';
    Fuzz::Say("line: %s", line.c_str());

    p.Parse(line.c_str());
    Expected e = Read(line);
    INVARIANT(p.IsParsed(), "the line not marked parsed");
    INVARIANT(e.Kept == p.GetCmdline(), "the line kept is '%s', not '%s'", p.GetCmdline(), e.Kept.c_str());
    INVARIANT(p.IsTraceVga() == e.TraceVga && p.IsPanicVga() == e.PanicVga && p.IsSmpOff() == e.SmpOff,
        "trace, panic or smp read wrong");
    INVARIANT(p.IsItsEnabled() == e.ItsEnabled && p.IsHwRngOff() == e.HwRngOff && p.IsUsbOff() == e.UsbOff &&
        p.IsRcOff() == e.RcOff, "its, hwrng, usb or rc read wrong");
    INVARIANT(p.IsWxProbeText() == (e.WxProbe == 1) && p.IsWxProbeHeap() == (e.WxProbe == 2), "wxprobe read wrong");
    INVARIANT(p.GetMaxCpus() == e.MaxCpus, "maxcpus %lu, not %lu", p.GetMaxCpus(), e.MaxCpus);
    INVARIANT(p.IsConsoleBoth() == (e.Console == 0) && p.IsConsoleSerial() == (e.Console == 1) &&
        p.IsConsoleVga() == (e.Console == 2), "console read wrong");
    INVARIANT(p.IsDhcpAuto() == (e.Dhcp == 1) && p.IsDhcpOff() == (e.Dhcp == 2), "dhcp read wrong");
    INVARIANT(p.GetUdpShellPort() == e.UdpShell, "udpshell %u, not %u", p.GetUdpShellPort(), e.UdpShell);
    INVARIANT(p.IsNetconsoleEnabled() == (e.NcPort != 0) && p.GetNetconsolePort() == e.NcPort &&
        p.GetNetconsoleIp() == e.NcIp, "netconsole 0x%x:%u, not 0x%x:%u", p.GetNetconsoleIp(), p.GetNetconsolePort(),
        e.NcIp, e.NcPort);
    INVARIANT(p.GetNetconsoleTailKb() == e.TailKb && p.GetNetFrameCount() == e.NetFrames, "nctail or netframes wrong");
    INVARIANT(p.IsRxPollEnabled() == e.RxPoll && p.IsDnsEnabled() == e.Dns && p.IsRootReadOnly() == e.Ro &&
        p.IsFsTest() == e.FsTest && p.IsDiskLogOn() == e.DiskLog, "rxpoll, dns, ro, fstest or disklog wrong");
    INVARIANT(p.GetLogLevel() == e.LogLevel, "loglevel %d, not %d", p.GetLogLevel(), e.LogLevel);
    const Parameters::RootSpec& root = p.GetRoot();
    INVARIANT(root.Mode == e.RootMode, "root mode %d, not %d", root.Mode, e.RootMode);
    INVARIANT(memchr(root.Value, 0, sizeof(root.Value)) != nullptr, "the root's value is unterminated");
    INVARIANT(e.RootValue == root.Value, "root '%s', not '%s'", root.Value, e.RootValue.c_str());
    /* The UUID is the root's only when it is one: a UUID refused may have
       left some of itself there, which nobody reads */
    if (root.Mode == Parameters::RootUuid)
        INVARIANT(memcmp(root.Uuid, e.RootUuid.data(), Parameters::UuidBytes) == 0, "the root's UUID is not the line's");
    if (e.UbsanSet)
        INVARIANT(Kernel::Ubsan::WarnOnly == e.UbsanWarn, "ubsan read wrong");
    Fuzz::Reached(line.size() >= Parameters::CmdlineLen ? "a line past what is kept" : "a line");
    if (e.RootMode == Parameters::RootUuid)
        Fuzz::Reached("root=UUID=");
    if (e.NcPort != 0)
        Fuzz::Reached("a netconsole");
}

void Reset()
{
    Parameters& p = Parameters::GetInstance();
    p.~Parameters();
    new (&p) Parameters();
    Kernel::Ubsan::WarnOnly = false;
}

}

const Fuzz::Target Fuzz::TheTarget = {"cmdline", Reset, Run, 1024, 100000};
