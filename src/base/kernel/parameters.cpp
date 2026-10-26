#include "parameters.h"
#include "cpu.h"
#include "panic.h"
#include "trace.h"
#include "ubsan.h"

namespace kernel
{

/* Bounds on netframes=N. The floor keeps a typo from starving the datapath;
   the ceiling keeps one from reserving a quarter of a gigabyte at boot, since
   the pool is built once and never grows. */
static const ulong NET_FRAME_COUNT_MIN = 64;
static const ulong NET_FRAME_COUNT_MAX = 65536;

/* Upper bound on nctail=N. The netconsole ring is smaller than this, and it
   treats a cap at or above its own size as no cap at all -- this only keeps
   a typo from being taken for a number. */
static const ulong NETCONSOLE_TAIL_KB_MAX = 1024;

/* hugepages=: a share in percent is at most the whole machine; a size is
   in MiB or GiB, and at most what the most RAM this kernel addresses is --
   past that it is a typo, not a machine. Either is cut down at boot to what
   the machine can spare (PageTable::reserve_huge_pool). */
static const ulong HUGE_POOL_PERCENT_MAX = 100;
static const ulong HUGE_POOL_MAX_GIB = 512;
static const ulong PERCENT_WHOLE = 100;
/* "1024G" and a NUL: the longest value worth reading. */
static const size_t HUGE_POOL_VALUE_MAX = 16;

Parameters::Parameters()
    : trace_vga_(false)
    , panic_vga_(false)
    , panic_reset_secs_(0)
    , smp_off_(false)
    , max_cpus_limit_(0)
    , its_enabled_(true)  /* PCIe MSI via GICv3 ITS is on by default; its=off to disable */
    , hw_rng_off_(false)
    , hwp_off_(false)
    , ucode_off_(false)
    , wx_probe_md_(WxProbeOff)
    , usb_off_(false)
    , rc_off_(false)
    , con_mode_(ConsoleBoth)
    , dhcp_md_(DhcpOn)
    , udp_shell_port_(0)
    , netconsole_ip_(0)
    , netconsole_port_(0)
    , netconsole_tail_kb_(0)
    , net_frame_count_(0)
    , huge_pool_percent_(0)
    , huge_pool_bytes_(0)
    , rx_poll_(false)
    , log_level_(DEFAULT_LOG_LEVEL)
    , dns_enabled_(false)
    , root_read_only_(false)
    , fs_test_(false)
    , disk_log_on_(false)
    , parsed_(false)
{
    cmdline_[0] = '\0';
    stdlib::mem_set(&root_, 0, sizeof(root_));
    root_.mode = RootNone;
}

Parameters::~Parameters()
{
}

bool Parameters::is_trace_vga()
{
    return trace_vga_;
}

bool Parameters::is_panic_vga()
{
    return panic_vga_;
}

ulong Parameters::get_panic_reset_secs()
{
    return panic_reset_secs_;
}

bool Parameters::is_smp_off()
{
    return smp_off_;
}

ulong Parameters::get_max_cpus()
{
    return max_cpus_limit_;
}

bool Parameters::is_hw_rng_off()
{
    return hw_rng_off_;
}

bool Parameters::is_hwp_off()
{
    return hwp_off_;
}

bool Parameters::is_ucode_off()
{
    return ucode_off_;
}

bool Parameters::is_its_enabled()
{
    return its_enabled_;
}

bool Parameters::is_wx_probe_text()
{
    return wx_probe_md_ == WxProbeText;
}

bool Parameters::is_wx_probe_heap()
{
    return wx_probe_md_ == WxProbeHeap;
}

bool Parameters::is_usb_off()
{
    return usb_off_;
}

bool Parameters::is_rc_off()
{
    return rc_off_;
}

bool Parameters::is_console_serial()
{
    return con_mode_ == ConsoleSerialOnly;
}

bool Parameters::is_console_vga()
{
    return con_mode_ == ConsoleVgaOnly;
}

bool Parameters::is_console_both()
{
    return con_mode_ == ConsoleBoth;
}

bool Parameters::is_dhcp_auto()
{
    return dhcp_md_ == DhcpAuto;
}

bool Parameters::is_dhcp_off()
{
    return dhcp_md_ == DhcpOff;
}

u16 Parameters::get_udp_shell_port()
{
    return udp_shell_port_;
}

bool Parameters::is_netconsole_enabled()
{
    return netconsole_port_ != 0;
}

u32 Parameters::get_netconsole_ip()
{
    return netconsole_ip_;
}

u16 Parameters::get_netconsole_port()
{
    return netconsole_port_;
}

ulong Parameters::get_netconsole_tail_kb()
{
    return netconsole_tail_kb_;
}

ulong Parameters::get_net_frame_count()
{
    return net_frame_count_;
}

ulong Parameters::get_huge_pool_bytes(ulong total_bytes)
{
    if (huge_pool_percent_ != 0)
    {
        /* Both bounded -- the percent by HUGE_POOL_PERCENT_MAX, the machine by
           what one PDPT maps -- so the product cannot wrap. */
        return total_bytes / PERCENT_WHOLE * huge_pool_percent_;
    }
    return huge_pool_bytes_;
}

/* "50%", "32G", "512M": a share of RAM or a size, into huge_pool_percent_ or
   huge_pool_bytes_. False, and neither set, for anything else. */
bool Parameters::parse_huge_pool(const char* value)
{
    const size_t len = stdlib::str_len(value);
    if (len < 2 || len >= HUGE_POOL_VALUE_MAX)
        return false;
    /* Everything but the last character, which says what the number is. */
    char digits[HUGE_POOL_VALUE_MAX];
    stdlib::strn_cpy(digits, value, len);
    ulong n = 0;
    if (!stdlib::parse_ulong(digits, n) || n == 0)
        return false;

    switch (value[len - 1])
    {
    case '%':
        if (n > HUGE_POOL_PERCENT_MAX)
            return false;
        huge_pool_percent_ = n;
        huge_pool_bytes_ = 0;
        return true;
    case 'G':
    case 'g':
        if (n > HUGE_POOL_MAX_GIB)
            return false;
        huge_pool_bytes_ = n * consts::GB;
        huge_pool_percent_ = 0;
        return true;
    case 'M':
    case 'm':
        if (n > HUGE_POOL_MAX_GIB * consts::GB / consts::MB)
            return false;
        huge_pool_bytes_ = n * consts::MB;
        huge_pool_percent_ = 0;
        return true;
    default:
        return false;
    }
}

bool Parameters::is_rx_poll_enabled()
{
    return rx_poll_;
}

int Parameters::get_log_level()
{
    return log_level_;
}

bool Parameters::is_dns_enabled()
{
    return dns_enabled_;
}

const Parameters::RootSpec& Parameters::get_root()
{
    return root_;
}

bool Parameters::is_root_read_only()
{
    return root_read_only_;
}

bool Parameters::is_fs_test()
{
    return fs_test_;
}

bool Parameters::is_disk_log_on()
{
    return disk_log_on_;
}

bool Parameters::is_parsed()
{
    return parsed_;
}

/* xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx, as blkid prints it */
bool Parameters::parse_uuid(const char* text, u8* out)
{
    if (stdlib::str_len(text) != UUID_TEXT_LEN)
        return false;

    char hex[UUID_BYTES * 2 + 1];
    ulong n = 0;
    for (ulong i = 0; i < UUID_TEXT_LEN; i++)
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
    return stdlib::hex_decode(hex, n, out, UUID_BYTES, bytes) && bytes == UUID_BYTES;
}

const char* Parameters::get_cmdline()
{
    return cmdline_;
}

/* A dotted quad into host byte order. The only thing the command line has
   ever needed of an address, and the reason this file used to include the
   whole network layer's header. */
static bool parse_ip_v4(const char* text, u32& out)
{
    u32 addr = 0;
    for (ulong octet = 0; octet < 4; octet++)
    {
        if (*text < '0' || *text > '9')
            return false;

        u32 value = 0;
        while (*text >= '0' && *text <= '9')
        {
            value = value * 10 + (u32)(*text - '0');
            if (value > 255)
                return false;
            text++;
        }
        addr = (addr << 8) | value;

        if (octet < 3)
        {
            if (*text != '.')
                return false;
            text++;
        }
    }

    if (*text != '\0')
        return false;

    out = addr;
    return true;
}

bool Parameters::parse_parameter(const char *cmdline, size_t start, size_t end)
{
    if (BUG_ON(start >= end))
        return false;

    char param[MAX_PARAM_LEN + 1];
    size_t len = end - start;
    if (len > MAX_PARAM_LEN)
        return false;

    /* The shortest token taken is the bare word ro */
    if (len < 2)
        return false;

    stdlib::strn_cpy(param, &cmdline[start], len + 1);

    /* The key ends at the first '=': the value may hold one of its own, as
       root=LABEL=<label> and root=UUID=<uuid> do */
    const char* sep = stdlib::str_chr(param, '=');
    if (sep == nullptr)
    {
        /* The one bare word taken, as on Linux: ro */
        if (stdlib::str_cmp(param, "ro") == 0)
        {
            trace(0, "Key %s", param);
            root_read_only_ = true;
            return true;
        }
        return false;
    }

    if ((sep == param) || (sep == &param[len - 1]))
        return false;

    size_t key_len = sep - param;
    const char *key = &param[0];
    param[key_len] = '\0';
    const char *value = &param[key_len + 1];

    trace(0, "Key %s value %s", key, value);

    if (stdlib::str_cmp(key, "trace") == 0)
    {
        if (stdlib::str_cmp(value, "vga") == 0)
        {
            trace_vga_ = true;
        }
        else
        {
            trace(0, "Unknown value %s, key %s", value, key);
        }
    }
    else if (stdlib::str_cmp(key, "panic") == 0)
    {
        ulong secs = 0;
        if (stdlib::str_cmp(value, "vga") == 0)
        {
            panic_vga_ = true;
        }
        else if (stdlib::parse_ulong(value, secs) && secs <= PANIC_RESET_SECS_MAX)
        {
            panic_reset_secs_ = secs;
        }
        else
        {
            trace(0, "Unknown value %s, key %s", value, key);
        }
    }
    else if (stdlib::str_cmp(key, "its") == 0)
    {
        its_enabled_ = (stdlib::str_cmp(value, "off") != 0);
    }
    else if (stdlib::str_cmp(key, "usb") == 0)
    {
        usb_off_ = (stdlib::str_cmp(value, "off") == 0);
    }
    else if (stdlib::str_cmp(key, "rc") == 0)
    {
        rc_off_ = (stdlib::str_cmp(value, "off") == 0);
    }
    else if (stdlib::str_cmp(key, "hwrng") == 0)
    {
        hw_rng_off_ = (stdlib::str_cmp(value, "off") == 0);
    }
    else if (stdlib::str_cmp(key, "hwp") == 0)
    {
        hwp_off_ = (stdlib::str_cmp(value, "off") == 0);
    }
    else if (stdlib::str_cmp(key, "ucode") == 0)
    {
        ucode_off_ = (stdlib::str_cmp(value, "off") == 0);
    }
    else if (stdlib::str_cmp(key, "wxprobe") == 0)
    {
        if (stdlib::str_cmp(value, "on") == 0 || stdlib::str_cmp(value, "text") == 0)
            wx_probe_md_ = WxProbeText;
        else if (stdlib::str_cmp(value, "heap") == 0)
            wx_probe_md_ = WxProbeHeap;
        else
            trace(0, "Unknown value %s, key %s", value, key);
    }
    else if (stdlib::str_cmp(key, "smp") == 0)
    {
        if (stdlib::str_cmp(value, "off") == 0)
        {
            smp_off_ = true;
        }
        else
        {
            trace(0, "Unknown value %s, key %s", value, key);
        }
    }
    else if (stdlib::str_cmp(key, "maxcpus") == 0)
    {
        ulong count = 0;
        if (stdlib::parse_ulong(value, count) && count > 0 && count <= MAX_CPUS)
        {
            max_cpus_limit_ = count;
        }
        else
        {
            trace(0, "Invalid maxcpus %s", value);
        }
    }
    else if (stdlib::str_cmp(key, "console") == 0)
    {
        if (stdlib::str_cmp(value, "serial") == 0)
        {
            con_mode_ = ConsoleSerialOnly;
        }
        else if (stdlib::str_cmp(value, "vga") == 0)
        {
            con_mode_ = ConsoleVgaOnly;
        }
        else if (stdlib::str_cmp(value, "both") == 0)
        {
            con_mode_ = ConsoleBoth;
        }
        else
        {
            trace(0, "Unknown value %s, key %s", value, key);
        }
    }
    else if (stdlib::str_cmp(key, "dhcp") == 0)
    {
        if (stdlib::str_cmp(value, "auto") == 0)
        {
            dhcp_md_ = DhcpAuto;
        }
        else if (stdlib::str_cmp(value, "on") == 0)
        {
            dhcp_md_ = DhcpOn;
        }
        else if (stdlib::str_cmp(value, "off") == 0)
        {
            dhcp_md_ = DhcpOff;
        }
        else
        {
            trace(0, "Unknown value %s, key %s", value, key);
        }
    }
    else if (stdlib::str_cmp(key, "udpshell") == 0)
    {
        ulong port = 0;
        if (stdlib::parse_ulong(value, port) && port > 0 && port <= 65535)
        {
            udp_shell_port_ = (u16)port;
        }
        else
        {
            trace(0, "Invalid udpshell port %s", value);
        }
    }
    else if (stdlib::str_cmp(key, "netconsole") == 0)
    {
        /* netconsole=ip:port */
        const char* colon = stdlib::str_chr_once(value, ':');
        if (colon == nullptr || colon == value || *(colon + 1) == '\0')
        {
            trace(0, "Invalid netconsole value %s, expected ip:port", value);
        }
        else
        {
            char ip_buf[16];
            size_t ip_len = colon - value;
            ulong port = 0;

            u32 ip = 0;
            if (ip_len >= sizeof(ip_buf))
            {
                trace(0, "Invalid netconsole ip in %s", value);
            }
            else
            {
                stdlib::strn_cpy(ip_buf, value, ip_len + 1);
                if (!parse_ip_v4(ip_buf, ip))
                {
                    trace(0, "Invalid netconsole ip %s", ip_buf);
                }
                else if (!stdlib::parse_ulong(colon + 1, port) || port == 0 || port > 65535)
                {
                    trace(0, "Invalid netconsole port %s", colon + 1);
                }
                else
                {
                    netconsole_ip_ = ip;
                    netconsole_port_ = (u16)port;
                }
            }
        }
    }
    else if (stdlib::str_cmp(key, "rxpoll") == 0)
    {
        rx_poll_ = (stdlib::str_cmp(value, "on") == 0);
    }
    else if (stdlib::str_cmp(key, "ubsan") == 0)
    {
        /* Meaningful in a UBSAN=1 build only, and harmless in any other. */
        ubsan::set_warn_only(stdlib::str_cmp(value, "warn") == 0);
    }
    else if (stdlib::str_cmp(key, "netframes") == 0)
    {
        ulong count = 0;
        if (stdlib::parse_ulong(value, count) &&
            count >= NET_FRAME_COUNT_MIN && count <= NET_FRAME_COUNT_MAX)
        {
            net_frame_count_ = count;
        }
        else
        {
            trace(0, "Invalid netframes %s", value);
        }
    }
    else if (stdlib::str_cmp(key, "hugepages") == 0)
    {
        if (!parse_huge_pool(value))
            trace(0, "Invalid hugepages %s: a share of RAM (50%%) or a size (32G, 512M)", value);
    }
    else if (stdlib::str_cmp(key, "nctail") == 0)
    {
        ulong kb = 0;
        if (stdlib::parse_ulong(value, kb) && kb > 0 && kb <= NETCONSOLE_TAIL_KB_MAX)
        {
            netconsole_tail_kb_ = kb;
        }
        else
        {
            trace(0, "Invalid nctail %s", value);
        }
    }
    else if (stdlib::str_cmp(key, "loglevel") == 0)
    {
        /* The runtime `loglevel` shell command cannot help with a boot that
           has already happened, and the chattier levels are exactly the ones
           worth having during bring-up on a machine you cannot log into. */
        ulong level = 0;
        if (stdlib::parse_ulong(value, level) && level <= (ulong)MAX_TRACE_LEVEL)
        {
            log_level_ = (int)level;
        }
        else
        {
            trace(0, "Invalid loglevel %s", value);
        }
    }
    else if (stdlib::str_cmp(key, "root") == 0)
    {
        const char* label_prefix = "LABEL=";
        const char* uuid_prefix = "UUID=";
        ulong label_prefix_len = stdlib::str_len(label_prefix);
        ulong uuid_prefix_len = stdlib::str_len(uuid_prefix);

        stdlib::mem_set(&root_, 0, sizeof(root_));
        if (stdlib::str_cmp(value, "auto") == 0)
        {
            root_.mode = RootAuto;
        }
        else if (stdlib::strn_cmp(value, label_prefix, label_prefix_len) == 0 &&
                 value[label_prefix_len] != '\0')
        {
            root_.mode = RootLabel;
            stdlib::strn_cpy(root_.value, value + label_prefix_len, sizeof(root_.value));
        }
        else if (stdlib::strn_cmp(value, uuid_prefix, uuid_prefix_len) == 0)
        {
            if (parse_uuid(value + uuid_prefix_len, root_.uuid))
            {
                root_.mode = RootUuid;
                stdlib::strn_cpy(root_.value, value + uuid_prefix_len, sizeof(root_.value));
            }
            else
            {
                trace(0, "Invalid UUID %s, key %s", value, key);
            }
        }
        else
        {
            root_.mode = RootDevice;
            stdlib::strn_cpy(root_.value, value, sizeof(root_.value));
        }
    }
    else if (stdlib::str_cmp(key, "fstest") == 0)
    {
        if (stdlib::str_cmp(value, "on") == 0)
        {
            fs_test_ = true;
        }
        else if (stdlib::str_cmp(value, "off") == 0)
        {
            fs_test_ = false;
        }
        else
        {
            trace(0, "Unknown value %s, key %s", value, key);
        }
    }
    else if (stdlib::str_cmp(key, "disklog") == 0)
    {
        if (stdlib::str_cmp(value, "on") == 0)
        {
            disk_log_on_ = true;
        }
        else if (stdlib::str_cmp(value, "off") == 0)
        {
            disk_log_on_ = false;
        }
        else
        {
            trace(0, "Unknown value %s, key %s", value, key);
        }
    }
    else if (stdlib::str_cmp(key, "dns") == 0)
    {
        if (stdlib::str_cmp(value, "on") == 0)
        {
            dns_enabled_ = true;
        }
        else
        {
            trace(0, "Unknown value %s, key %s", value, key);
        }
    }
    else
    {
        trace(0, "Unknown key %s, skipping", key);
    }

    return true;
}

void Parameters::parse(const char *cmdline)
{
    /* Not set until the whole line has been read, at the end. Parsing traces
       a line per parameter, and the disk log keeps a line only while nothing
       is known yet or disklog=on has been seen: set here, the parameters
       before disklog on the line would be traced into the gap between the
       two and lost from the one channel a machine with no serial port has.

       No line at all is an answer too: nothing on it was asked for, and the
       flag goes up on that path as well. */

    if (cmdline == nullptr)
    {
        parsed_ = true;
        return;
    }

    /* strn_cpy, not sn_printf: past the end of the buffer sn_printf keeps
       nothing, and what is wanted there is as much as fits */
    stdlib::strn_cpy(cmdline_, cmdline, sizeof(cmdline_));
    ulong kept = stdlib::str_len(cmdline_);
    if (cmdline[kept] != '\0')
    {
        /* Longer than the buffer. The parameter the end cuts through goes
           as well: half of one is a different one, and root=LABEL=nosenv
           cut short is root=LABEL=nos. */
        if (cmdline[kept] != ' ')
        {
            while (kept > 0 && cmdline_[kept - 1] != ' ')
                kept--;
            cmdline_[kept] = '\0';
        }

        const char* rest = &cmdline[kept];
        while (*rest == ' ')
            rest++;

        char shown[MAX_PARAM_LEN + 1];
        stdlib::strn_cpy(shown, rest, sizeof(shown));
        trace(0, "Cmdline: longer than %lu characters, ignoring from '%s'",
            (ulong)(CMDLINE_LEN - 1), shown);
    }

    ulong len = stdlib::str_len(cmdline_);
    ulong start = 0;
    for (ulong i = 0; i <= len; i++)
    {
        if (i < len && cmdline_[i] != ' ')
            continue;

        if (start < i && !parse_parameter(cmdline_, start, i))
        {
            /* Named, so that a typo, or a Linux habit such as quiet, is a
               line in the log rather than a boot that stops */
            ulong shown_len = i - start;
            if (shown_len > MAX_PARAM_LEN)
                shown_len = MAX_PARAM_LEN;

            char shown[MAX_PARAM_LEN + 1];
            stdlib::strn_cpy(shown, &cmdline_[start], shown_len + 1);
            if (i - start > MAX_PARAM_LEN)
                trace(0, "Cmdline: ignoring '%s...', longer than the %lu characters "
                    "a parameter can have", shown, (ulong)MAX_PARAM_LEN);
            else
                trace(0, "Cmdline: ignoring '%s'", shown);
        }
        start = i + 1;
    }

    parsed_ = true;
}

}
