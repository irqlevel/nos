# Shell commands

The interactive shell runs on the serial console and the screen, and every
command also works over the [UDP remote shell](udp-shell.md). Trace output is
suppressed while the shell is active (it still lands in `dmesg`).

| Command | Description |
|---------|-------------|
| `cls` | Clear screen |
| `cpu` | Dump CPU state |
| `lscpu` | Identify the CPU and report the features the kernel depends on (1 GiB pages, NX, PAT, invariant TSC, ARAT), the performance counters it has (Intel architectural perfmon, or AMD core counters and PerfMonV2), and which of them `profile` will actually sample with |
| `dmesg [lines] [filter]` | Dump kernel log: newest `lines` messages (all of it if omitted), optional substring filter |
| `loglevel [N]` | Show or set the trace level (0-5) on a running kernel |
| `uptime` | Show uptime |
| `date` | Show wall clock date and time (RTC + boot time) |
| `igbdump` | The igb chip's own state: MAC command/status registers, ring head and tail, receive statistics, and the PHY's view of the link — what it advertised, what the partner offered, both 1000BASE-T registers. What made the I210's 10 Mb/s latch diagnosable |
| `nicdump` | The r8125's own state: command register (is the receiver still enabled?), interrupt status and mask, and whose the head receive descriptor is. For a machine that has stopped receiving and can still be typed at |
| `netload [start [port] [sink]\|stop\|reset]` | UDP load target, so `profile` has something to look at other than an idle machine: echoes every datagram from the receive softirq (or drops it, in sink mode) and counts packets, bytes and rate per CPU. Drive it with `scripts/netload.py` (`--pps` to hold a rate, `--threads` to raise the ceiling) |
| `stacks` | Stack high-water marks: every stack is filled with a pattern when it is created, and what is still intact is what it never reached. Catches a spike that lasted microseconds during boot, and costs nothing while the machine runs |
| `ps` | Show tasks |
| `top [ms]` | Per-task CPU use over a sampling window, percent per CPU (a busy thread reads 100%, a 20-CPU box tops out at 2000%), plus the number of tasks moved between CPU queues since boot |
| `profile [ms] [pid\|all] [chains]` | Sampling profiler: where the kernel spends its time, as whole call chains -- samples are folded by their entire stack, not by the leaf symbol, and the hottest chains are printed in full. `chains` caps how many, for a console with no scrollback -- `profile 2000 all 3` fits a screen where the default does not. Every frame carries its offset: the leaf as the instruction the sample landed on (a span when they spread across the body, so a hot spinlock says whether it sat on the exchange or in the pause loop), each caller as the return address that names which call site led there. Samples on a performance counter overflowing into an NMI (~1 kHz, and catches code running with interrupts off) where the CPU has one -- Intel's architectural fixed counter 1, or AMD's PMCx076 on family 15h and later, both counting unhalted core cycles; falls back to the 100 Hz per-CPU tick where it does not, which includes any machine whose hypervisor answers CPUID for a PMU it declines to virtualise (the counter is asked to prove it counts before the profiler trusts it). The report names which |
| `bt <pid>` | Dump stack trace of a task (uses IPI for remote CPUs) |
| `watchdog` | Watchdog stats: locks watched, table walks, and the slice of the bucket table each CPU walks (the table is divided among the CPUs, so a bucket is visited once per tick rather than once per tick per CPU) |
| `memusage` | Show memory usage |
| `meminfo` | Show the firmware memory map, and how much of it the kernel actually uses |
| `memcheck` | Verify no reserved, kernel-image or absent page reached the free list |
| `pci` | Show PCI devices |
| `disks` | List block devices |
| `diskread <disk> <sector>` | Read and hex-dump a sector |
| `diskwrite <disk> <sector> <hex>` | Write hex data to a sector |
| `partitions <disk>` | Show the partition table (MBR or GPT) |
| `disklog` | Kernel-log-to-disk state: the prepared area it found (if any), boot number, sectors written, lines dropped. The area is laid down under the host OS with `scripts/disklog.py format` and read back with `scripts/disklog.py read` |
| `irqstat` | Show per-device interrupt counters |
| `help` | List commands |
| `net` | List network devices and per-protocol stats |
| `arp` | Show ARP table |
| `netpool` | Show the recycled net frame pool: frames in the ring, in per-CPU caches, in flight |
| `netconsole` | Show netconsole target, buffered bytes, drop/send counters |
| `icmpstat` | Show ICMP statistics |
| `tcpstat` | Show TCP connections and statistics |
| `wget <url>` | Fetch a URL via HTTP GET (follows redirects) |
| `udpsend <ip> <port> <msg>` | Send a UDP packet |
| `ping <ip\|hostname>` | Send 5 ICMP echo requests with RTT (resolves hostnames via DNS) |
| `nslookup <hostname>` | Resolve hostname to IP via DNS |
| `dnsflush` | Flush DNS cache |
| `dhcp [dev]` | Obtain IP address via DHCP |
| `random [len]` | Get random bytes as hex string |
| `format nanofs <disk>` | Format disk with nanofs |
| `mount ramfs <path>` | Mount a ramfs at path |
| `mount nanofs <disk> <path>` | Mount nanofs from disk at path |
| `umount <path>` | Unmount filesystem |
| `mounts` | List mount points |
| `ls <path>` | List directory contents |
| `cat <path>` | Show file contents |
| `write <path> <text>` | Write text to file (creates if needed) |
| `mkdir <path>` | Create directory |
| `touch <path>` | Create empty file |
| `del <path>` | Remove file or directory |
| `usb` | Show xHCI controllers, connected root ports and keyboard report counters |
| `panic [type]` | Trigger kernel panic (direct, pagefault, divzero, ud) |
| `version` | Show kernel version |
| `poweroff` / `shutdown` | Power off (ACPI S5) |
| `reboot` | Reset system (keyboard controller) |
