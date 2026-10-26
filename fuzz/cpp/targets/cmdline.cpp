// The kernel command line (kernel/parameters.cpp): what GRUB's config, the
// device tree's bootargs or QEMU's -append hand the kernel before anything
// else runs, and the first thing a boot on a machine with no serial port
// acts on -- the netconsole to report to, the root to mount, the CPUs to
// bring up. A line is built from the input of the parameters
// docs/kernel-parameters.md names, with values right and wrong, of words
// nobody named, of tokens past the longest one taken and of lines past the
// longest one kept, and handed to Parameters::parse; every getter is held to
// a reader of the line written from parameters.h: tokens split on spaces, a
// parameter longer than MAX_PARAM_LEN skipped, a line past CMDLINE_LEN - 1 read
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

using kernel::Parameters;

namespace kernel
{
namespace ubsan
{

/* ubsan=warn: the one parameter that reaches into another subsystem */
bool warn_only;

void set_warn_only(bool on)
{
    warn_only = on;
}

}
}

namespace
{

/* ---- the model: parameters.h's reading of a line ---- */

struct Expected
{
    bool trace_vga = false, panic_vga = false, smp_off = false, its_enabled = true, hw_rng_off = false;
    int wx_probe = 0; /* off, text, heap */
    bool usb_off = false, rc_off = false, hwp_off = false, ucode_off = false;
    ulong panic_reset_secs = 0;
    int console = 0; /* both, serial, vga */
    int dhcp = 0;    /* on, auto, off */
    ulong max_cpus = 0, net_frames = 0, tail_kb = 0;
    u16 udp_shell = 0, nc_port = 0;
    u32 nc_ip = 0;
    bool rx_poll = false, dns = false, ro = false, fs_test = false, disk_log = false;
    int log_level = Parameters::DEFAULT_LOG_LEVEL;
    int root_mode = Parameters::RootNone;
    std::string root_value;
    std::vector<uint8_t> root_uuid = std::vector<uint8_t>(Parameters::UUID_BYTES, 0);
    bool ubsan_warn = false;
    bool ubsan_set = false;
    /* hugepages=: a share in percent, or bytes -- the last one said */
    ulong huge_percent = 0, huge_bytes = 0;
    std::string kept;
};

bool number(const std::string& s, unsigned long long& out)
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

bool parse_ip(const std::string& s, u32& out)
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
        if (!number(p, v) || v > 255)
            return false;
        addr = (addr << 8) | static_cast<u32>(v);
    }
    out = addr;
    return true;
}

bool uuid(const std::string& s, std::vector<uint8_t>& out)
{
    if (s.size() != Parameters::UUID_TEXT_LEN)
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
    out.assign(Parameters::UUID_BYTES, 0);
    for (size_t i = 0; i < Parameters::UUID_BYTES; i++)
        out[i] = static_cast<uint8_t>(std::stoul(hex.substr(2 * i, 2), nullptr, 16));
    return true;
}

void take(Expected& e, const std::string& token)
{
    if (token.size() > Parameters::MAX_PARAM_LEN || token.size() < 2)
        return;
    size_t eq = token.find('=');
    if (eq == std::string::npos)
    {
        if (token == "ro")
            e.ro = true;
        return;
    }
    if (eq == 0 || eq == token.size() - 1)
        return;
    std::string key = token.substr(0, eq), value = token.substr(eq + 1);
    unsigned long long n;
    if (key == "trace" && value == "vga")
        e.trace_vga = true;
    else if (key == "panic")
    {
        if (value == "vga")
            e.panic_vga = true;
        else if (number(value, n) && n <= Parameters::PANIC_RESET_SECS_MAX)
            e.panic_reset_secs = n;
    }
    else if (key == "its")
        e.its_enabled = value != "off";
    else if (key == "usb")
        e.usb_off = value == "off";
    else if (key == "rc")
        e.rc_off = value == "off";
    else if (key == "hwrng")
        e.hw_rng_off = value == "off";
    else if (key == "hwp")
        e.hwp_off = value == "off";
    else if (key == "ucode")
        e.ucode_off = value == "off";
    else if (key == "wxprobe")
    {
        if (value == "on" || value == "text")
            e.wx_probe = 1;
        else if (value == "heap")
            e.wx_probe = 2;
    }
    else if (key == "smp" && value == "off")
        e.smp_off = true;
    else if (key == "maxcpus")
    {
        if (number(value, n) && n > 0 && n <= kernel::MAX_CPUS)
            e.max_cpus = n;
    }
    else if (key == "console")
    {
        if (value == "serial")
            e.console = 1;
        else if (value == "vga")
            e.console = 2;
        else if (value == "both")
            e.console = 0;
    }
    else if (key == "dhcp")
    {
        if (value == "auto")
            e.dhcp = 1;
        else if (value == "on")
            e.dhcp = 0;
        else if (value == "off")
            e.dhcp = 2;
    }
    else if (key == "udpshell")
    {
        if (number(value, n) && n > 0 && n <= 65535)
            e.udp_shell = static_cast<u16>(n);
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
        if (ip.size() >= 16 || !parse_ip(ip, addr))
            return;
        if (!number(value.substr(colon + 1), n) || n == 0 || n > 65535)
            return;
        e.nc_ip = addr;
        e.nc_port = static_cast<u16>(n);
    }
    else if (key == "rxpoll")
        e.rx_poll = value == "on";
    else if (key == "ubsan")
    {
        e.ubsan_set = true;
        e.ubsan_warn = value == "warn";
    }
    else if (key == "netframes")
    {
        if (number(value, n) && n >= 64 && n <= 65536)
            e.net_frames = n;
    }
    else if (key == "nctail")
    {
        if (number(value, n) && n > 0 && n <= 1024)
            e.tail_kb = n;
    }
    else if (key == "hugepages")
    {
        /* A number and what it is: % of RAM, G or M of it; at most 100%,
           512 GiB, and a value shorter than 16 */
        if (value.size() < 2 || value.size() >= 16 || !number(value.substr(0, value.size() - 1), n) || n == 0)
            return;
        const char unit = value.back();
        if (unit == '%' && n <= 100)
        {
            e.huge_percent = n;
            e.huge_bytes = 0;
        }
        else if ((unit == 'G' || unit == 'g') && n <= 512)
        {
            e.huge_bytes = n << 30;
            e.huge_percent = 0;
        }
        else if ((unit == 'M' || unit == 'm') && n <= 512 * 1024)
        {
            e.huge_bytes = n << 20;
            e.huge_percent = 0;
        }
    }
    else if (key == "loglevel")
    {
        if (number(value, n) && n <= static_cast<unsigned long long>(kernel::MAX_TRACE_LEVEL))
            e.log_level = static_cast<int>(n);
    }
    else if (key == "root")
    {
        /* Whatever it says, what was said before is gone */
        e.root_mode = Parameters::RootNone;
        e.root_value.clear();
        e.root_uuid.assign(Parameters::UUID_BYTES, 0);
        if (value == "auto")
        {
            e.root_mode = Parameters::RootAuto;
        }
        else if (value.compare(0, 6, "LABEL=") == 0 && value.size() > 6)
        {
            e.root_mode = Parameters::RootLabel;
            e.root_value = value.substr(6, Parameters::ROOT_VALUE_LEN - 1);
        }
        else if (value.compare(0, 5, "UUID=") == 0)
        {
            if (uuid(value.substr(5), e.root_uuid))
            {
                e.root_mode = Parameters::RootUuid;
                e.root_value = value.substr(5, Parameters::ROOT_VALUE_LEN - 1);
            }
            else
            {
                e.root_uuid.assign(Parameters::UUID_BYTES, 0);
            }
        }
        else
        {
            e.root_mode = Parameters::RootDevice;
            e.root_value = value.substr(0, Parameters::ROOT_VALUE_LEN - 1);
        }
    }
    else if (key == "fstest")
    {
        if (value == "on")
            e.fs_test = true;
        else if (value == "off")
            e.fs_test = false;
    }
    else if (key == "disklog")
    {
        if (value == "on")
            e.disk_log = true;
        else if (value == "off")
            e.disk_log = false;
    }
    else if (key == "dns" && value == "on")
    {
        e.dns = true;
    }
}

Expected read(const std::string& line)
{
    Expected e;
    /* What is kept of the line: CMDLINE_LEN - 1 characters, and if that cuts
       through a parameter, only up to the one before it */
    const size_t keep = Parameters::CMDLINE_LEN - 1;
    std::string kept = line.substr(0, keep);
    if (line.size() > keep && line[keep] != ' ')
    {
        size_t space = kept.rfind(' ');
        kept = (space == std::string::npos) ? std::string() : kept.substr(0, space + 1);
    }
    e.kept = kept;
    size_t at = 0;
    while (at <= kept.size())
    {
        size_t space = kept.find(' ', at);
        size_t end = (space == std::string::npos) ? kept.size() : space;
        if (end > at)
            take(e, kept.substr(at, end - at));
        at = end + 1;
    }
    return e;
}

/* ---- the line ---- */

const char* const WORDS[] = {"trace", "panic", "its", "usb", "rc", "hwrng", "hwp", "ucode", "wxprobe", "smp",
                             "maxcpus", "console", "dhcp", "udpshell", "netconsole", "rxpoll", "ubsan", "netframes",
                             "nctail", "loglevel", "root", "fstest", "disklog", "dns", "hugepages"};

const char* const HUGE_VALUES[] = {"50%", "100%", "101%", "0%", "1%", "32G", "512G", "513G", "0G", "512M", "524288M",
                                  "524289m", "1g", "2m", "G", "%", "M", "50", "50%%", "0050%", "%50", "18446744073709551615G",
                                  "99999999999999G", "9999999999999M", "5x", "-1G"};

const char* const BAD_IPS[] = {"1.2.3", "1.2.3.4.5", "01.2.3.4", "1..2.3", "a.b.c.d", "1.2.3.4:5",
                              "255.255.255.255"};

const char* const STRAYS[] = {"quiet", "splash", "r", "=", "a=", "=b", "foo=bar", "nokaslr", "console=ttyS0,115200",
                              "ro=1"};

const char* const VALUES[] = {"on", "off", "vga", "serial", "both", "auto", "text", "heap", "warn", "", "0", "1",
                              "64", "65", "65535", "65536", "1024", "1025", "3600", "3601", "5", "6",
                              "18446744073709551615", "18446744073709551616", "-1", "1x", "abc"};

std::string value(fuzz::Input& in, const std::string& key)
{
    switch (in.u8() % 8)
    {
    case 0:
        return in.pick(VALUES);
    case 1:
        return std::to_string(in.value32());
    default:
        break;
    }
    if (key == "netconsole")
    {
        std::string ip = std::to_string(in.below(300)) + "." + std::to_string(in.below(256)) + "." +
                         std::to_string(in.below(256)) + "." + std::to_string(in.below(260));
        if (in.chance(24))
            ip = in.pick(BAD_IPS);
        return ip + (in.chance(16) ? "" : ":") + std::to_string(in.below(70000));
    }
    if (key == "hugepages")
    {
        if (in.chance(128))
            return in.pick(HUGE_VALUES);
        const char* units[] = {"%", "G", "M", "g", "m", "K", ""};
        return std::to_string(in.chance(16) ? in.value32() : in.below(600)) + in.pick(units);
    }
    if (key == "root")
    {
        switch (in.u8() % 5)
        {
        case 0:
            return "auto";
        case 1:
            return "LABEL=" + std::string(in.below(45), 'l');
        case 2:
        {
            std::string u = "UUID=";
            for (int i = 0; i < 36; i++)
                u += (i == 8 || i == 13 || i == 18 || i == 23) ? '-' : "0123456789abcdefABCDEFg"[in.below(in.chance(8) ? 23 : 22)];
            if (in.chance(16))
                u.pop_back();
            return u;
        }
        default:
            return std::string("vd") + static_cast<char>('a' + in.below(4)) + (in.boolean() ? "1" : "");
        }
    }
    return in.pick(VALUES);
}

std::string token(fuzz::Input& in)
{
    switch (in.u8() % 12)
    {
    case 0:
        return "ro";
    case 1:
    {
        /* A word nobody named, a Linux habit */
        return in.pick(STRAYS);
    }
    case 2:
        /* Past the longest parameter taken, and at it */
        return "root=" + std::string(in.range(40, 50), 'x');
    case 3:
    {
        std::string t;
        for (int i = in.below(12); i > 0; i--)
            t += static_cast<char>(in.chance(32) ? in.range(1, 255) : 'a' + in.below(26));
        return t;
    }
    default:
    {
        std::string key = in.pick(WORDS);
        return key + "=" + value(in, key);
    }
    }
}

void run(fuzz::Input& in)
{
    Parameters& p = Parameters::get_instance();
    if (in.chance(4))
    {
        p.parse(nullptr);
        INVARIANT(p.is_parsed(), "no line, and the parameters not parsed");
        return;
    }
    std::string line;
    for (int n = in.chance(32) ? 40 : in.below(12); n > 0; n--)
    {
        std::string sep(in.chance(32) ? in.range(2, 4) : 1, ' ');
        line += (line.empty() ? std::string(in.chance(16) ? 1 : 0, ' ') : sep) + token(in);
    }
    if (in.chance(8))
        line += ' ';
    fuzz::say("line: %s", line.c_str());

    p.parse(line.c_str());
    Expected e = read(line);
    INVARIANT(p.is_parsed(), "the line not marked parsed");
    INVARIANT(e.kept == p.get_cmdline(), "the line kept is '%s', not '%s'", p.get_cmdline(), e.kept.c_str());
    INVARIANT(p.is_trace_vga() == e.trace_vga && p.is_panic_vga() == e.panic_vga && p.is_smp_off() == e.smp_off,
        "trace, panic or smp read wrong");
    INVARIANT(p.get_panic_reset_secs() == e.panic_reset_secs, "panic=%lu, not %lu", p.get_panic_reset_secs(), e.panic_reset_secs);
    INVARIANT(p.is_hwp_off() == e.hwp_off && p.is_ucode_off() == e.ucode_off, "hwp or ucode read wrong");
    INVARIANT(p.is_its_enabled() == e.its_enabled && p.is_hw_rng_off() == e.hw_rng_off && p.is_usb_off() == e.usb_off &&
        p.is_rc_off() == e.rc_off, "its, hwrng, usb or rc read wrong");
    INVARIANT(p.is_wx_probe_text() == (e.wx_probe == 1) && p.is_wx_probe_heap() == (e.wx_probe == 2), "wxprobe read wrong");
    INVARIANT(p.get_max_cpus() == e.max_cpus, "maxcpus %lu, not %lu", p.get_max_cpus(), e.max_cpus);
    INVARIANT(p.is_console_both() == (e.console == 0) && p.is_console_serial() == (e.console == 1) &&
        p.is_console_vga() == (e.console == 2), "console read wrong");
    INVARIANT(p.is_dhcp_auto() == (e.dhcp == 1) && p.is_dhcp_off() == (e.dhcp == 2), "dhcp read wrong");
    INVARIANT(p.get_udp_shell_port() == e.udp_shell, "udpshell %u, not %u", p.get_udp_shell_port(), e.udp_shell);
    INVARIANT(p.is_netconsole_enabled() == (e.nc_port != 0) && p.get_netconsole_port() == e.nc_port &&
        p.get_netconsole_ip() == e.nc_ip, "netconsole 0x%x:%u, not 0x%x:%u", p.get_netconsole_ip(), p.get_netconsole_port(),
        e.nc_ip, e.nc_port);
    INVARIANT(p.get_netconsole_tail_kb() == e.tail_kb && p.get_net_frame_count() == e.net_frames, "nctail or netframes wrong");
    INVARIANT(p.is_rx_poll_enabled() == e.rx_poll && p.is_dns_enabled() == e.dns && p.is_root_read_only() == e.ro &&
        p.is_fs_test() == e.fs_test && p.is_disk_log_on() == e.disk_log, "rxpoll, dns, ro, fstest or disklog wrong");
    INVARIANT(p.get_log_level() == e.log_level, "loglevel %d, not %d", p.get_log_level(), e.log_level);
    /* The pool asked of a machine of some size: the share of it, or the
       bytes, whichever the line said last */
    {
        const ulong total = (in.chance(64) ? (512UL << 30) : (static_cast<ulong>(in.range(1, 1024)) << 26));
        const ulong want = e.huge_percent != 0 ? total / 100 * e.huge_percent : e.huge_bytes;
        INVARIANT(p.get_huge_pool_bytes(total) == want, "hugepages= asks %lu bytes of %lu, not %lu",
            p.get_huge_pool_bytes(total), total, want);
        if (want != 0)
            fuzz::reached(e.huge_percent != 0 ? "hugepages= a share" : "hugepages= a size");
    }
    const Parameters::RootSpec& root = p.get_root();
    INVARIANT(root.mode == e.root_mode, "root mode %d, not %d", root.mode, e.root_mode);
    INVARIANT(memchr(root.value, 0, sizeof(root.value)) != nullptr, "the root's value is unterminated");
    INVARIANT(e.root_value == root.value, "root '%s', not '%s'", root.value, e.root_value.c_str());
    /* The UUID is the root's only when it is one: a UUID refused may have
       left some of itself there, which nobody reads */
    if (root.mode == Parameters::RootUuid)
        INVARIANT(memcmp(root.uuid, e.root_uuid.data(), Parameters::UUID_BYTES) == 0, "the root's UUID is not the line's");
    if (e.ubsan_set)
        INVARIANT(kernel::ubsan::warn_only == e.ubsan_warn, "ubsan read wrong");
    fuzz::reached(line.size() >= Parameters::CMDLINE_LEN ? "a line past what is kept" : "a line");
    if (e.root_mode == Parameters::RootUuid)
        fuzz::reached("root=UUID=");
    if (e.nc_port != 0)
        fuzz::reached("a netconsole");
}

void reset()
{
    Parameters& p = Parameters::get_instance();
    p.~Parameters();
    new (&p) Parameters();
    kernel::ubsan::warn_only = false;
}

}

const fuzz::Target fuzz::THE_TARGET = {"cmdline", reset, run, 1024, 100000};
