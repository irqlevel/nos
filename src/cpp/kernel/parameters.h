#pragma once

#include <lib/stdlib.h>

namespace kernel
{

class Parameters
{
public:
    static Parameters& get_instance()
    {
        static Parameters instance;
        return instance;
    }

    /* Take what can be taken and name what cannot. A parameter that cannot be
       parsed is logged and skipped and the rest of the line still read, as
       Linux does, and a line longer than CMDLINE_LEN - 1 characters is read
       up to its last whole parameter. Nothing here stops the boot: this runs
       before the netconsole, the disk log or the NIC exist, so on a machine
       whose only console is the network a boot that stopped here would stop
       without a word. */
    void parse(const char *cmdline);

    /* Whether parse() has run. Before it every getter reports its default,
       and a caller that must not act on a default -- the disk log, keeping
       the first lines of the boot until it knows -- can tell. */
    bool is_parsed();

    /* The line, its terminator included, and the longest single parameter
       taken -- room for root=UUID=<uuid>, which is 46 */
    static const ulong CMDLINE_LEN = 256;
    static const ulong MAX_PARAM_LEN = 48;

    bool is_trace_vga();
    bool is_panic_vga();

    /* panic=N -- reset the machine N seconds after a panic's report, 1 to
       PANIC_RESET_SECS_MAX; 0, the default, keeps it stopped with the report
       on its screen. For a machine whose only console is the network,
       whose next boot is its own OS: back there with no hand on it. */
    ulong get_panic_reset_secs();
    static const ulong PANIC_RESET_SECS_MAX = 3600;
    bool is_smp_off();

    /* maxcpus=N -- bring up at most N CPUs, the BSP included. 0 = no cap. */
    ulong get_max_cpus();

    bool is_its_enabled();

    /* wxprobe=text (or the older =on): write to .text once the image has
       been split, wxprobe=heap: call into a page the allocator just handed
       out. Both are meant to be fatal -- they are how W^X is shown to be
       doing its job (docs/paging.md). */
    bool is_wx_probe_text();
    bool is_wx_probe_heap();

    bool is_usb_off();

    /* rc=off -- do not run /etc/rc at boot: the way past a command in it
       that keeps the machine from coming up (see Cmd::run_boot_script) */
    bool is_rc_off();

    /* hwrng=off -- ignore the cpu's random instruction (RDRAND/RDSEED,
       RNDR). For seeing what a machine without one does, on a machine that
       has one; the pool then falls back on virtio-rng and timing jitter. */
    bool is_hw_rng_off();

    /* hwp=off -- x86: leave the CPUs' clock where firmware left it rather
       than turning on hardware P-states (hal::setup_cpu_performance) */
    bool is_hwp_off();

    /* ucode=off -- x86: load no microcode from the module GRUB put beside
       the kernel; the CPUs run firmware's (arch/x86_64/microcode.h) */
    bool is_ucode_off();

    bool is_console_serial();
    bool is_console_vga();
    bool is_console_both();

    bool is_dhcp_auto();
    bool is_dhcp_off();

    u16 get_udp_shell_port();

    /* netconsole=ip:port -- stream the kernel log to that collector */
    bool is_netconsole_enabled();
    /* Host byte order; 0 when no netconsole was asked for. */
    u32 get_netconsole_ip();
    u16 get_netconsole_port();

    /* nctail=N -- when the link comes up, ship only the newest N KiB of the
       buffered boot log. 0 (the default) ships all of it. */
    ulong get_netconsole_tail_kb();

    /* netframes=N -- how many frames the network pool is built with. 0 (the
       default) means the network layer's own default. Worth having as a knob
       rather than a rebuild: how many frames a driver keeps in flight is a
       property of that driver and that load, and the only way to find out is
       to raise it on the machine in question and watch `netpool` for misses. */
    ulong get_net_frame_count();

    /* hugepages=N% | NG | NM -- the huge pool (mm/page_table.h,
       reserve_huge_pool): that share of the machine's RAM, or that much of
       it, reserved at boot in 2MiB blocks for guests' memory, each block
       one entry of a guest's nested page table. 0, the default, reserves
       none, and a guest's memory is 4KiB frames as it always was; with a
       pool, a guest's memory is the pool's or the guest does not start.
       What is asked past what the machine can spare is cut down to it, and
       the boot says so. The bytes for a machine of total_bytes of RAM. */
    ulong get_huge_pool_bytes(ulong total_bytes);

    /* rxpoll=on -- have the tick look at the receive path as well as the
       NIC's own interrupt. Off by default: it is a hypothesis about a driver
       stall, not a proven fix, and on the machine it was meant to help the
       stall arrived sooner with it on than with it off. Left behind a switch
       so the two can be compared without a rebuild, which is the only way to
       tell on a box whose console is a UDP socket. The network layer's
       poll asks (kernel_param_rxpoll_on); the tick only offers. */
    bool is_rx_poll_enabled();

    /* loglevel=N: the trace level to boot with. Defaults to what main sets
       today; the `loglevel` shell command moves it afterwards. */
    int get_log_level();
    static const int DEFAULT_LOG_LEVEL = 1;

    bool is_dns_enabled();

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

    static const ulong ROOT_VALUE_LEN = 40;
    static const ulong UUID_BYTES = 16;
    static const ulong UUID_TEXT_LEN = 36;

    struct RootSpec {
        RootMode mode;
        char value[ROOT_VALUE_LEN];
        u8 uuid[UUID_BYTES];
    };

    const RootSpec& get_root();

    /* ro: mount the root filesystem read-only */
    bool is_root_read_only();

    /* fstest=on: run the filesystem self-test on / once it is mounted */
    bool is_fs_test();

    /* disklog=on: write the kernel log to a disk area prepared with
       scripts/disklog.py (see kernel/disklog.h). Off by default: it costs
       a forced disk write per burst of lines. */
    bool is_disk_log_on();

    const char* get_cmdline();

    Parameters();
    ~Parameters();
private:
    bool parse_parameter(const char *cmdline, size_t start, size_t end);

    enum ConsoleMode {
        ConsoleBoth = 0,
        ConsoleSerialOnly,
        ConsoleVgaOnly,
    };

    enum WxProbeMode {
        WxProbeOff = 0,
        WxProbeText,   /* write to .text */
        WxProbeHeap,   /* execute from a page the heap handed out */
    };

    enum DhcpMode {
        DhcpOn = 0,    /* start only by cmd (default) */
        DhcpAuto,      /* start automatically at boot */
        DhcpOff,       /* disabled entirely */
    };

    char cmdline_[CMDLINE_LEN];
    bool trace_vga_;
    bool panic_vga_;
    ulong panic_reset_secs_;
    bool smp_off_;
    ulong max_cpus_limit_;
    bool its_enabled_;
    bool hw_rng_off_;
    bool hwp_off_;
    bool ucode_off_;
    WxProbeMode wx_probe_md_;
    bool usb_off_;
    bool rc_off_;
    ConsoleMode con_mode_;
    DhcpMode dhcp_md_;
    u16 udp_shell_port_;
    u32 netconsole_ip_;
    u16 netconsole_port_;
    ulong netconsole_tail_kb_;
    ulong net_frame_count_;
    /* hugepages=: a share of RAM in percent, or bytes -- one of the two. */
    ulong huge_pool_percent_;
    ulong huge_pool_bytes_;
    bool rx_poll_;
    int log_level_;
    bool dns_enabled_;
    RootSpec root_;
    bool root_read_only_;
    bool fs_test_;
    bool disk_log_on_;
    bool parsed_;

    static bool parse_uuid(const char* text, u8* out);
    bool parse_huge_pool(const char* value);
};
}