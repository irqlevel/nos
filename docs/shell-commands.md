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
| `top [ms]` | Per-task CPU use over a sampling window, percent per CPU (a busy thread reads 100%, a 20-CPU box tops out at 2000%), plus the number of tasks moved between CPU queues since boot, and of reschedules deferred because the task they landed on had preemption off -- how many of those on an idle task |
| `profile [ms] [pid\|all] [chains]` | Sampling profiler: where the kernel spends its time, as whole call chains -- samples are folded by their entire stack, not by the leaf symbol, and the hottest chains are printed in full. `chains` caps how many, for a console with no scrollback -- `profile 2000 all 3` fits a screen where the default does not. Every frame carries its offset: the leaf as the instruction the sample landed on (a span when they spread across the body, so a hot spinlock says whether it sat on the exchange or in the pause loop), each caller as the return address that names which call site led there. Samples on a performance counter overflowing into an NMI (~1 kHz, and catches code running with interrupts off) where the CPU has one -- Intel's architectural fixed counter 1, or AMD's PMCx076 on family 15h and later, both counting unhalted core cycles; falls back to the 100 Hz per-CPU tick where it does not, which includes any machine whose hypervisor answers CPUID for a PMU it declines to virtualise (the counter is asked to prove it counts before the profiler trusts it). The report names which |
| `bt <pid>` | Dump stack trace of a task (uses IPI for remote CPUs) |
| `watchdog` | Watchdog stats: locks watched, table walks, and the slice of the bucket table each CPU walks (the table is divided among the CPUs, so a bucket is visited once per tick rather than once per tick per CPU) |
| `memusage` | Show memory usage |
| `meminfo` | Show the firmware memory map, and how much of it the kernel actually uses |
| `memcheck` | Verify no reserved, kernel-image or absent page reached the free list |
| `pci` | Show PCI devices |
| `disks` | List block devices |
| `diskread <disk> <sector>` | Read and hex-dump a sector |
| `diskwrite <disk> <sector> <hex>` | Write hex data to a sector. Refused on a device a mounted filesystem, the disk log or a `blkload` write test holds, or on a disk or partition overlapping one (see [Filesystems](filesystems.md)) |
| `partitions <disk>` | Show the partition table (MBR or GPT) |
| `disklog` | Kernel-log-to-disk state: off unless the kernel was booted with `disklog=on`; otherwise the prepared area it found (if any), boot number, sectors written, lines queued and dropped. The area is laid down under the host OS with `scripts/disklog.py format` and read back with `scripts/disklog.py read` |
| `irqstat` | Show per-device interrupt counters |
| `help` | List commands |
| `net` | List network devices and per-protocol stats |
| `arp` | Show ARP table |
| `netpool` | Show the recycled net frame pool: frames in the ring, in per-CPU caches, in flight |
| `netconsole` | Show netconsole target, buffered bytes, drop/send counters |
| `icmpstat` | Show ICMP statistics |
| `tcpstat` | Show TCP connections and statistics |
| `wget [-o <path>] <url> [path]` | Fetch a URL via HTTP or HTTPS GET (follows redirects, including http→https). With a path — `-o` and the trailing argument are the same thing — the body is streamed to that file as it arrives, up to 20 MiB, with a progress line every megabyte. Without one it is printed, and kept in memory, so it is capped at 32 KiB. HTTPS verifies the certificate chain against the built-in roots and the wall clock; see [HTTPS](tls.md) |
| `udpsend <ip> <port> <msg>` | Send a UDP packet |
| `ping <ip\|hostname>` | Send 5 ICMP echo requests with RTT (resolves hostnames via DNS) |
| `nslookup <hostname>` | Resolve hostname to IP via DNS |
| `dnsflush` | Flush DNS cache |
| `dhcp [dev]` | Obtain IP address via DHCP |
| `random [len]` | Get random bytes as hex string, 1..1024, default 16 — from the kernel's ChaCha20 pool, not from a device |
| `entropy [reseed]` | Show the random pool (seeded, whether hardware entropy reached it, reseeds, bytes generated) and the registered entropy sources; `reseed` draws from every source again first. See [Randomness](random.md) |
| `format nanofs <disk>` | Format disk with nanofs. Refused, like `diskwrite`, on a device in use |
| `mount ramfs <path>` | Mount a ramfs at path |
| `mount nanofs <disk> <path>` | Mount nanofs from disk at path |
| `mount ext2 <disk> <path> [ro]` | Mount ext2 from disk at path, read-write unless `ro` (or unless the image carries features the driver does not write; see [Filesystems](filesystems.md)) |
| `umount <path>` | Unmount filesystem |
| `mounts` | List mount points |
| `ls [path]` | List directory contents (default `/`) |
| `cp [-r] <src> <dst>` | Copy a file, or a whole directory tree with `-r`; a `dst` that is an existing directory takes the copy under its own name |
| `cat <path>` | Show file contents |
| `write <path> <text>` | Write text to file (creates if needed) |
| `mkdir <path>` | Create directory |
| `touch <path>` | Create empty file |
| `rm <path>` | Remove file or directory (recursively); `del` is the same command |
| `append <path> <text>` | Append text to a file (created if missing) |
| `mv <old> <new>` | Rename or move a file or directory within one filesystem |
| `stat <path>` | Type, size and inode number |
| `sync` | Flush every mounted filesystem to disk |
| `fstest [dir] [size]` | Filesystem self-test in `dir` (default `/`) with a big file of `size` bytes (`K`/`M` suffix; default 300 KiB); `fstest / 5M` reaches the doubly-indirect blocks at 4 KiB blocks |
| `crc32 <path>` | CRC-32 of a file, to check a copy against the host |
| `sha256 <path>` | SHA-256 of a file, printed the way `sha256sum` prints it, to check a downloaded kernel against the `SHA256SUMS` of its release |
| `grubenv <path> [name=value ...]` | Show, or set, the variables of a GRUB environment block — the `grubenv` file `grub-editenv` makes and GRUB's `load_env`/`save_env` read and write; `name=` with nothing after it removes one. Edited in place at the same size, the way GRUB's own writer edits it, so GRUB can still read and clear what was set. What arms a one-shot boot of a downloaded kernel from inside nos: see [Real hardware](real-hardware.md#updating-the-kernel-from-inside-nos) |
| `insmod <path>` | Load a kernel module — a Rust crate built into a `.ko` by `make modules` — from a file, and run its init, in a task of its own. Says why when it refuses one: not a module, another machine's, built against another kernel interface, an import the kernel does not export, a name already loaded or still loading or unloading. A load not done in five seconds carries on in the background, the kernel log saying how it ended. See [Loadable modules](modules.md) |
| `rmmod <name>` | Unload a module in a task of its own: run its exit, which releases everything it holds, then free its code. The exit waits for a call of one of the module's commands still running; past five seconds the shell leaves it to finish in the background |
| `blkload <dev> [randread\|randwrite\|read\|write] [bs=4k] [qd=1] [secs=5] [force]` | From the `blkload` module, once it is loaded: a short load test on a disk or a partition — IOPS, bandwidth, latency min/avg/p50/p90/p99/p99.9/max. A write test claims the device first: refused while a mounted filesystem, the disk log or another write test holds it — or the disk it is on, or one of its partitions — and without `force` on a disk with partitions or a device holding a filesystem, a partition table or a disk log area. See [Loadable modules](modules.md#blkload) |
| `netblk start <disk> <port> [ro] [nic=eth0] [mtu=1500] [cpu=N] [poll=us]` | From the `netblk` module, once it is loaded: serve an NVMe disk, or a partition of one, over UDP on `port` (1024 and up, one instance to a port), zero-copy. Read-write claims the device the way a `blkload` write test does; `ro` takes no claim. A port someone else has is refused. `poll` keeps the worker spinning that long for the next request before it sleeps. See [netblk](netblk.md) |
| `netblk list` / `netblk stop <port>\|all` | The instances served and their counters; stop one, or all -- waiting for what the disk still has in flight |
| `lsmod` | List the modules: name, size, load address, how many kernel functions each binds to, and which are permanent, still loading or still unloading. Never waits on a load or unload in progress |
| `rc [add <line>\|del <n>\|clear\|run]` | Show or edit `/etc/rc`: shell commands, a line each (blank lines and `#` comments skipped), that the shell's task runs once at boot after DHCP -- how a module such as [sshd](sshd.md) gets loaded and started on a machine nobody can reach until it is. `rc` numbers the lines, `add` appends one, `del` takes one out, `clear` removes the file, `run` runs it now. What it prints at boot goes to the kernel log and the screen; `rc=off` skips it |
| `usb` | Show xHCI controllers, connected root ports and keyboard report counters |
| `panic [type]` | Trigger kernel panic (direct, pagefault, divzero, ud) |
| `version` | Show kernel version |
| `poweroff` / `shutdown` | Power off (ACPI S5) |
| `reboot` | Reset system (keyboard controller) |
